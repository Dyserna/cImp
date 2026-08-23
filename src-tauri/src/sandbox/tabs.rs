//! V33 **Phase B** — sandboxing the AI-tool tab itself.
//!
//! Phase A confined the children an agent asks cImp to run. This confines the
//! agent. The engine is unchanged (Windows AppContainer, spike S1); what Phase B
//! adds is a pseudoconsole in the same `STARTUPINFOEXW` attribute list (spike
//! S3, `docs/reviews/SPIKE-S3-conpty-appcontainer-2026-08-18.md`) and the
//! per-harness grant table below. The Win32 half lives in
//! [`crate::pty::sandboxed_conpty`]; this module is the policy half, and it is
//! deliberately platform-neutral so the table and the env rules are reviewable
//! (and testable) on any machine.
//!
//! # Scope: AI-tool tabs only (decision B1)
//!
//! Everything that goes through `tabs::config::build_ai_tool_spec` — Claude,
//! claude-local, OpenCode. **Plain Shell tabs are never sandboxed and mint no
//! row.** A shell tab is the user's own hands at their own machine, not a seam a
//! model's request reaches; confining it would be cImp deciding what its user
//! may do, which is a different product than this one.
//!
//! # Network is unconditional here (decision B3)
//!
//! `sandbox.allow_network` is `run_command`'s knob and does **not** govern tabs.
//! A sandboxed tab ALWAYS gets `internetClient`, because an AI CLI that cannot
//! reach its own model endpoint is not a hardened tab, it is a broken one — and
//! a boundary users switch off because it bricks their tool protects nobody.
//! The honest granularity is still all-or-nothing: per-host scoping is WFP work
//! (V36 / spike S4), and until it exists a sandboxed tab reaches the internet
//! *and* the LAN, exactly as `allow_network = true` does elsewhere.
//!
//! # What the boundary costs, stated once
//!
//! A sandboxed tab can read+write the project root and its own harness state,
//! read the OS and its own program files — and nothing else. Deliberately NOT
//! granted (decision B5): `~/.ssh`, the Windows Credential Manager, other
//! projects, cImp's own settings. So a `git push` from inside a sandboxed tab is
//! refused, and **that refusal is the boundary being honest** rather than a
//! defect: a tab that could reach the user's credentials was never confined in
//! the first place. Users widen it deliberately through
//! `sandbox.extra_grant_dirs`, which is honored for tabs like everywhere else.
//!
//! # The accepted identity consequence (decision B6)
//!
//! The child's cwd is the mapped `subst` drive root, not the real project path —
//! the same mitigation Phase A uses, and load-bearing here because a tab runs
//! `git` constantly (S1's `mingw_getcwd` gotcha). The consequence, accepted with
//! eyes open: **a sandboxed Claude tab sees `S:\…` as its project path**, so
//! Claude Code keys its per-project state (`~/.claude/projects/<slug>`) under a
//! different slug than the same tab unsandboxed. Turning the switch on or off
//! therefore looks to Claude like moving to a different project — history and
//! per-project settings do not follow. This is not a bug to fix here; removing
//! it means removing the drive mapping, which re-breaks git.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{GrantAccess, GrantHints, GrantRow, SandboxCfg};

/// Which AI harness a tab runs — the registry's [`HarnessId`].
///
/// Chosen by `tabs::config::build_ai_tool_spec` (the one place that already
/// knows), carried on `PtyLaunchSpec`, and used here for exactly one thing:
/// asking that harness's plugin for its grant table.
///
/// **V40 Phase A deleted the local enum.** It was the second `Harness` type in
/// the tree and it had the same two variants as the first, which is one variant
/// per harness in two places — the shape this milestone exists to end. A tab
/// whose command matches no registered harness still gets `None` on the spec and
/// is not sandboxed at all, because a grant table nobody wrote is not a
/// boundary; it is a tool that fails to start for reasons the user cannot see.
///
/// **[`crate::harness::HarnessId::ANY`] is NOT a sandbox harness** (V40 review
/// L-5). The old local enum made it unrepresentable; `HarnessId` does not, and
/// `ANY` has no descriptor, so it would sandbox a tab with the neutral rows and
/// **no harness state grants at all** — precisely the "fails to start for
/// reasons the user cannot see" outcome the paragraph above exists to avoid.
/// Unreachable today (`from_command` never answers `ANY`), and
/// [`grant_rows_with`] debug-asserts it rather than leaving the next caller to
/// rediscover it.
pub use crate::harness::HarnessId as Harness;

/// The runtime sandbox config for a TAB spawn.
///
/// Two deliberate departures from [`SandboxCfg::from_settings`], each a locked
/// decision rather than a convenience:
///
/// * `enabled` is `sandbox.enabled && sandbox.tabs` (B2) — `tabs` is a scope
///   widener inside the OS layer, never a second master switch;
/// * `allow_network` is forced ON (B3) — see the module header.
///
/// `extra_grant_dirs` carries through untouched: a user who listed a toolchain
/// directory meant it for every sandboxed child, and making them list it twice
/// would be a distinction with no meaning behind it.
pub fn tab_sandbox_cfg(s: &crate::settings::Settings) -> SandboxCfg {
    let mut cfg = SandboxCfg::from_settings(s);
    cfg.enabled = s.sandbox.enabled && s.sandbox.tabs;
    cfg.allow_network = true;
    cfg
}

/// Which switch left this tab unsandboxed — the note that rides the skip row's
/// detail so the user is not left hunting between two checkboxes.
///
/// Empty when the tab IS being sandboxed (nothing to explain) and when the
/// reason is `Unavailable` rather than a choice… except that an `Unavailable`
/// can only be reached with both switches on, so this returning `""` there is
/// the same statement.
pub fn off_note(s: &crate::settings::Settings) -> &'static str {
    match (s.sandbox.enabled, s.sandbox.tabs) {
        (false, false) => {
            "Sandboxing is off entirely (Settings ▸ Sandboxing ▸ master switch), and AI tabs \
             are additionally not included."
        }
        (false, true) => {
            "AI-tab sandboxing is switched ON but the Sandboxing master switch is OFF, so it \
             has no effect — the master switch governs the OS boundary for every seam."
        }
        (true, false) => {
            "Sandboxing is on for the commands agents run, but NOT for AI tabs \
             (Settings ▸ Sandboxing ▸ “Also sandbox AI tabs”)."
        }
        (true, true) => "",
    }
}

// ── the grant table (decision B5) ────────────────────────────────────────────

/// The rows cImp's **own** proxy child needs — and the reason the directory
/// above them never becomes one.
///
/// Since V37 Phase F every AI tab, Claude and OpenCode alike, is handed an MCP
/// server whose command is cImp's own executable (`cimp --offload-mcp --tab ...`;
/// the same binary also backs the conditional `--code-audit-mcp` child). The
/// harness spawns that child from INSIDE the container, so without these rows it
/// cannot be started at all — and if it started it could not find the app: it
/// resolves the loopback port + token from
/// `<exe-dir>/.cimp-discovery/<pid>.json`, falling back to the legacy
/// `<exe-dir>/.cimp-offload.json` (`offload::discovery::select_discovery`). Each
/// of those is a denial row, which is loud — and still a broken tab.
///
/// **Three file-scoped rows, never the directory.** `<exe-dir>` is cImp's
/// portable root: `settings.json` (backend URLs, API keys, every auth token the
/// app holds), `tool-activity.jsonl`, the detection stores. A read+execute ACE
/// on that directory would hand a compromised agent every secret cImp has —
/// exactly what decision B5 exists to prevent — so this is the same shape the
/// `~/.claude.json` row uses to reach a file without opening `%USERPROFILE%`.
///
/// The discovery TOKEN is readable by the child, deliberately: it is the
/// credential the child authenticates to its own app with, and every tab
/// carrying this child has always read it. `settings.json` beside it stays dark,
/// which is the line that matters.
///
/// Two residuals, written down rather than papered over:
///
/// * the legacy `.cimp-offload.json` is an OPTIONAL row, so a tab prepared
///   before the app ever wrote that file gets no ACE on it and nothing inherits
///   one (its directory is ungranted). The per-instance entry under the granted
///   `.cimp-discovery/` is the authoritative path and DOES inherit, so the child
///   still resolves; only the legacy fallback would be missing.
/// * a sibling DLL that cImp's binary imports at LOAD time would be unreadable
///   for the same reason. The shipped layout has none — the one non-system
///   load-time import, `DirectML.dll`, resolves from `System32`, which every app
///   package can already read, and the GPU DLLs beside the exe are
///   `LoadLibrary`-ed by GUI paths this headless child never runs.
///
/// Also deliberately absent: `<exe-dir>/tool-activity.jsonl`. The child appends
/// to it only in the app-not-running fallback, and a sandboxed tab by
/// construction has a running app to forward to; a write ACE on cImp's own
/// activity log is not worth a fallback that cannot happen here.
///
/// Linux: nothing to do. V33 Phase D confines the three tool seams and
/// deliberately not tabs (`portable_pty` exposes no `pre_exec` hook — see
/// [`plan_tab`]), so this table has no Linux consumer.
fn cimp_child_rows(exe: Option<&Path>) -> Vec<GrantRow> {
    // No `current_exe()` (or an executable at a filesystem root): no guesses.
    // The tab still launches and the child fails as a denial row, which is the
    // same degradation every other absent row gets.
    let (Some(exe), Some(dir)) = (exe, exe.and_then(Path::parent)) else {
        return Vec::new();
    };
    vec![
        GrantRow {
            path: exe.to_path_buf(),
            access: GrantAccess::ReadExecute,
            is_file: true,
            reason: "cImp's own executable, which the harness spawns as its `cimp-offload` MCP \
                     server. A FILE grant: the directory around it is cImp's portable root and \
                     holds settings.json, so it is never granted as a whole",
            required: false,
        },
        GrantRow {
            path: dir.join(crate::offload::discovery::DISCOVERY_FILE),
            access: GrantAccess::ReadExecute,
            is_file: true,
            reason: "the legacy discovery file the proxy child reads to find this app's loopback \
                     port and token. Read-only, and file-scoped so settings.json beside it stays \
                     unreadable",
            required: false,
        },
        GrantRow {
            path: dir.join(crate::offload::discovery::DISCOVERY_DIR),
            access: GrantAccess::ReadExecute,
            is_file: false,
            reason: "the per-instance discovery directory (<pid>.json per running instance), how \
                     the proxy child finds THIS instance's loopback rather than a sibling's. It \
                     holds discovery entries and nothing else, and the grant is read-only",
            required: false,
        },
    ]
}

/// Where a harness keeps its own state, as data with a reason per row.
///
/// **Read this as a security review, not as configuration.** Every row widens
/// what a compromised agent can read, so every row answers: what is it, why does
/// the tool break without it, and why is this width the smallest one that works.
///
/// What is NOT here, and stays not here:
///
/// * **`~/.ssh`** — the whole point. A tab that can read the user's private keys
///   is not confined. A refused `git push` is the boundary working.
/// * **The Windows Credential Manager** — same, and it is not a filesystem grant
///   anyway (an AppContainer gets its own credential store by construction).
/// * **`%USERPROFILE%` itself** — granting the parent to reach two files in it
///   would hand over the entire home directory, which is why the two Claude
///   config files are FILE grants (see [`GrantRow::is_file`]).
/// * **cImp's own install directory** — the same argument, applied to cImp: the
///   tab needs cImp's *binary* and its *discovery data*, and gets exactly those
///   as file-scoped rows. See [`cimp_child_rows`] for what else is in that
///   directory and must stay dark.
///
/// What a user adds themselves, through `sandbox.extra_grant_dirs`: package
/// manager caches (`~/.bun`, `%LOCALAPPDATA%\npm-cache`) if their harness
/// installs plugins at startup, and any toolchain the agent shells out to that
/// does not live under Program Files. Those surface first as a **denial row**,
/// which is the design — the lane names what the boundary refused, and the user
/// decides whether to widen it.
fn grant_rows_with(
    harness: Harness,
    env: &dyn Fn(&str) -> Option<OsString>,
    exe: Option<&Path>,
) -> Vec<GrantRow> {
    // See the `Harness` alias: `ANY` is a type-valid value with no descriptor,
    // so it would confine a tab with cImp's neutral rows and none of the harness
    // state its program needs (V40 review L-5). A `PtyLaunchSpec` carrying it is
    // a bug in the caller, not a configuration.
    debug_assert!(
        harness.id().is_some(),
        "`HarnessId::ANY` reached the tab sandbox: it has no grant table, so the tab would be          confined with no harness state and fail to start for reasons the user cannot see"
    );
    // cImp's own three first: they are anchored on the executable, not on the
    // home directory, so a machine that reports no home still gets a working
    // proxy child.
    let mut rows = cimp_child_rows(exe);
    let Some(home) = env("USERPROFILE").or_else(|| env("HOME")).map(PathBuf::from) else {
        // No home directory to anchor the HARNESS rows on: skip them rather than
        // guessing. The tab still gets its project root, its program's install
        // dir and the rows above, and whatever it then cannot read shows up as a
        // denial row.
        return rows;
    };
    // XDG spelling first where the harness honors it, then the default —
    // OpenCode reads `XDG_CONFIG_HOME`/`XDG_DATA_HOME` on Windows too, and a
    // user who relocated them would otherwise get a boundary around the wrong
    // directories with no clue why.
    let xdg = |var: &str, default: &[&str]| -> PathBuf {
        env(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| default.iter().fold(home.clone(), |p, seg| p.join(seg)))
    };

    // Shared by both harnesses: git identity. An AI tab commits, and a commit
    // with no `user.name` fails with a message about running `git config`, which
    // is a confusing way to discover a sandbox boundary.
    rows.extend([
        GrantRow {
            path: home.join(".gitconfig"),
            access: GrantAccess::ReadExecute,
            is_file: true,
            reason: "git identity for commits made from this tab (read-only: the tab commits, \
                     it does not reconfigure the user's git)",
            required: false,
        },
        GrantRow {
            path: xdg("XDG_CONFIG_HOME", &[".config"]).join("git"),
            access: GrantAccess::ReadExecute,
            is_file: false,
            reason: "git's XDG config location, for users who keep their identity there \
                     instead of in ~/.gitconfig",
            required: false,
        },
    ]);

    // Where THIS harness keeps its own state, declared by its own plugin
    // (V40 locked decision 4). Read an implementation as a security review, not
    // as configuration: every row widens what a compromised agent can read.
    rows.extend(harness.plugin().map(|p| {
        p.sandbox_grants(&crate::harness::plugin::GrantCtx { home: &home, env })
    }).unwrap_or_default());
    rows
}

/// [`grant_rows_with`] against the real process environment and this process's
/// own executable.
pub fn grant_hints(harness: Harness) -> GrantHints {
    let exe = std::env::current_exe().ok();
    GrantHints {
        // A harness is spawned by cImp, not declared by a manifest: inference.
        runtime: super::RuntimeSelect::Infer,
        programs: Vec::new(),
        full_dirs: Vec::new(),
        rows: grant_rows_with(harness, &|k| std::env::var_os(k), exe.as_deref()),
    }
}

// ── the child's scratch (decision B4) ────────────────────────────────────────

/// `<root>/.cimp/sandbox-tmp/<tab id>` — the only `TEMP`/`TMP` a sandboxed tab
/// gets.
///
/// Inside the project root because that is the one place the boundary makes
/// writable; per tab because two tabs sharing a temp directory is how one tab's
/// half-written file becomes the other's mysterious parse error. Under
/// `.cimp/`, which cImp already owns in a project (`.cimp/config.json`,
/// `.cimp/shadow.git`) and which this repo's `.gitignore` covers with `**/.cimp/`
/// — a project that does NOT ignore it would see the directory in `git status`,
/// the same way it already would for the two files above.
///
/// `allow(dead_code)` off Windows for the same reason the rest of this layer
/// carries it. Unlike the rest, this one is still literally true after V33
/// Phase D: the Landlock engine confines the three TOOL seams and deliberately
/// not tabs (see [`plan_tab`]), so the AppContainer engine remains this
/// function's only consumer.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn scratch_dir(root: &Path, tab_id: &str) -> PathBuf {
    root.join(".cimp").join("sandbox-tmp").join(sanitize(tab_id))
}

/// Tab ids come from settings and are user-editable, so they are not trusted to
/// be a single path segment. Anything outside `[A-Za-z0-9._-]` becomes `_`, and
/// an id that is empty or all dots (`.`, `..` — the two names that would climb
/// out of the scratch directory) becomes `tab`.
fn sanitize(tab_id: &str) -> String {
    let s: String = tab_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        "tab".to_string()
    } else {
        s
    }
}

/// The environment overrides a sandboxed tab gets **on top of** the environment
/// the plain spawn would have built (decision B4).
///
/// Exactly two entries, and the list is short because every addition is a way
/// for the sandboxed tab to behave differently from the unsandboxed one:
///
/// * `TEMP` / `TMP` → the per-tab scratch inside the root, because the real
///   `%TEMP%` is outside the boundary and a tool that cannot write a temp file
///   fails in ways that read as anything but a sandbox.
///
/// **`HOME` and `USERPROFILE` stay REAL, unlike `run_command`'s** — which is the
/// single most important line in this file. `run_command` redirects them because
/// its children are short-lived probes with no state worth keeping; a tab CLI's
/// entire identity (credentials, session history, per-project state) lives under
/// the real home, and redirecting it would give the user a tab that is logged
/// out, remembers nothing, and looks broken rather than confined. The grant
/// table above is what makes that safe: the home DIRECTORY stays unreadable;
/// only the harness's own state within it is granted.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn env_overrides(scratch: &Path) -> Vec<(String, OsString)> {
    let value = scratch.as_os_str().to_os_string();
    vec![
        ("TEMP".to_string(), value.clone()),
        ("TMP".to_string(), value),
    ]
}

// ── the decision (degradation semantics, B9) ─────────────────────────────────

/// How one tab's launch is going to run.
pub enum TabPlan {
    /// Inside the container. Carries the [`Prepared`](super::windows::Prepared)
    /// whose `DriveGuard` must outlive the PTY session (decision B10) — boxed so
    /// the enum stays small on the plain path.
    #[cfg(windows)]
    Sandboxed(Box<super::windows::Prepared>),
    /// Outside it, loudly (a skip row is already recorded).
    Plain,
    /// **Do not launch.** Preparation wedged; the string is user-facing and the
    /// tab shows it as its launch error. Never a silent unsandboxed fallback:
    /// dropping the boundary because a step hung is exactly the degradation
    /// decision 5 forbids.
    Refused(String),
}

/// Decide (and prepare) one AI tab's sandbox.
///
/// Mirrors `run_command`'s Phase A structure exactly, including the caller-side
/// [`PREPARE_BACKSTOP`](super::PREPARE_BACKSTOP): preparation is a blocking Win32
/// dance that has wedged twice in this codebase's history (rc.6's `map_drive`
/// self-deadlock, rc.4's drain race), and a path whose only deadline lives inside
/// itself has no deadline at all.
///
/// The three outcomes and their row:
///
/// | outcome | row | tab |
/// |---|---|---|
/// | sandboxed | confirmation, once per tab per session (minted by the caller after the spawn) | launches confined |
/// | switch off / prerequisite missing | one skip row, deduped per (seam, reason) | launches plain |
/// | preparation wedged | a `wedged` row | **does not launch** |
pub async fn plan_tab(
    settings: &crate::settings::Settings,
    harness: Harness,
    tab_id: &str,
    program: &Path,
    root: &Path,
) -> TabPlan {
    let cfg = tab_sandbox_cfg(settings);
    let seam = super::tab_seam(tab_id);
    let hints = grant_hints(harness);
    let no_env: &[(&str, OsString)] = &[];
    let planned = tokio::time::timeout(
        super::PREPARE_BACKSTOP,
        super::plan(&cfg, &seam, program, &hints, root, no_env),
    )
    .await;
    let plan = match planned {
        Ok(plan) => plan,
        Err(_) => {
            super::record_event(
                &seam,
                root,
                "wedged",
                super::state_target("wedged", tab_id),
                format!(
                    "sandbox preparation for the {} tab `{tab_id}` did not settle within {}s \
                     (profile / ACL grants / drive mapping). The tab was NOT launched — refusing \
                     rather than silently dropping the sandbox boundary. The preparation thread \
                     may still be blocked; if this repeats, restart cImp and read this lane for \
                     what preceded it.",
                    harness.label(),
                    super::PREPARE_BACKSTOP.as_secs(),
                ),
                false,
            );
            return TabPlan::Refused(format!(
                "sandbox preparation did not settle within {}s — treating as wedged (see the \
                 Sandboxing lane in Events); the tab was not launched",
                super::PREPARE_BACKSTOP.as_secs()
            ));
        }
    };
    match plan {
        #[cfg(windows)]
        super::Plan::Sandboxed(prepared) => {
            // The child's only writable scratch (decision B4). Created here,
            // inside preparation, so the tab never starts with a `TEMP` that
            // does not exist — a tool that cannot write a temp file fails in
            // ways that read as anything but a sandbox.
            let scratch = scratch_dir(root, tab_id);
            if let Err(e) = std::fs::create_dir_all(&scratch) {
                let reason = super::SkipReason::Unavailable(format!(
                    "could not create the sandbox scratch directory {} ({e})",
                    scratch.display()
                ));
                super::record_skip_noting(&seam, &reason, tab_id, root, off_note(settings));
                // Dropping `prepared` releases the drive mapping it took.
                return TabPlan::Plain;
            }
            TabPlan::Sandboxed(Box::new(prepared))
        }
        // V33 Phase D: the Linux engine exists and confines the three TOOL
        // seams, but a tab is spawned by `portable_pty` and there is nowhere to
        // apply `restrict_self` in its child. Investigated, precisely:
        // `CommandBuilder` exposes no exec hook (its public surface is args,
        // env, cwd, umask, controlling_tty), its `as_command()` — the one place
        // a `std::process::Command` exists — is `pub(crate)`, and
        // `UnixPtyPair::spawn_command` registers its OWN `pre_exec` (signal
        // dispositions, `setsid`, `TIOCSCTTY`, umask) before handing back a
        // child. `pre_exec` closures compose in registration order, so a
        // `CommandBuilder::pre_exec` hook upstream — or a public
        // `as_command()` — would be enough; today neither exists, and the
        // alternative is reimplementing the unix PTY spawn the way
        // `pty::sandboxed_conpty` reimplements the Windows one. That is a phase
        // of its own, not a line here.
        //
        // So the ruleset this arm was handed is dropped and the tab launches
        // plain — LOUDLY, as its own skip row, never as an empty lane that
        // reads like confinement.
        #[cfg(target_os = "linux")]
        super::Plan::Sandboxed(_) => {
            let reason = super::SkipReason::Unavailable(
                "AI tabs are not sandboxed on Linux: the PTY backend (portable_pty) exposes no \
                 spawn hook to apply a Landlock ruleset in the child. The run_command, run_check \
                 and audit seams ARE confined on this machine."
                    .to_string(),
            );
            super::record_skip_noting(&seam, &reason, tab_id, root, off_note(settings));
            TabPlan::Plain
        }
        super::Plan::Plain(reason) => {
            super::record_skip_noting(&seam, &reason, tab_id, root, off_note(settings));
            TabPlan::Plain
        }
    }
}

#[cfg(test)]
mod tests {

    /// The two shipped harnesses, resolved through the registry — the tests may
    /// name a harness, they just may not construct one.
    fn claude() -> Harness {
        Harness::from_id("claude").expect("claude is registered")
    }
    fn opencode() -> Harness {
        Harness::from_id("opencode").expect("opencode is registered")
    }
    /// Every registered harness, for the tests that assert a property of all of
    /// them rather than of a named one.
    fn every_harness() -> Vec<Harness> {
        crate::harness::registry::all().collect()
    }
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).map(OsString::from)
    }

    const HOME: &str = r"C:\Users\tester";

    /// Forward slashes on purpose (V35's CI lesson): a backslash literal is a
    /// SINGLE path component on the Linux runner, so `Path::parent` there would
    /// return an empty directory and every `<exe-dir>/…` expectation below would
    /// hold locally and fail in CI. [`paths`] normalizes the separator back.
    const EXE: &str = "C:/cimp/bin/cimp.exe";
    const EXE_DIR: &str = r"C:\cimp\bin";

    fn rows(harness: Harness, pairs: &[(&str, &str)]) -> Vec<GrantRow> {
        let e = env_of(pairs);
        grant_rows_with(harness, &e, Some(Path::new(EXE)))
    }

    /// The three rows [`cimp_child_rows`] mints, in order, as `paths` spells
    /// them.
    fn cimp_paths() -> Vec<String> {
        vec![
            format!(r"{EXE_DIR}\cimp.exe"),
            format!(r"{EXE_DIR}\.cimp-offload.json"),
            format!(r"{EXE_DIR}\.cimp-discovery"),
        ]
    }

    fn paths(rows: &[GrantRow]) -> Vec<String> {
        rows.iter()
            .map(|r| r.path.to_string_lossy().replace('/', "\\"))
            .collect()
    }

    /// The Claude table, as shipped. Pinned by path AND width, because a row
    /// silently widening from read-only to read+write is the change nobody
    /// notices in a diff full of doc comments.
    #[test]
    fn the_claude_grant_table_is_what_it_claims() {
        let rows = rows(claude(), &[("USERPROFILE", HOME)]);
        let by_path: Vec<(String, GrantAccess, bool)> = rows
            .iter()
            .map(|r| {
                (
                    r.path.to_string_lossy().replace('/', "\\"),
                    r.access,
                    r.is_file,
                )
            })
            .collect();
        assert_eq!(
            by_path,
            vec![
                // cImp's own three (V37): the binary the harness spawns as its
                // `cimp-offload` MCP server, and the discovery data that child
                // reads to find this app. File-scoped — see `cimp_child_rows`.
                (
                    format!(r"{EXE_DIR}\cimp.exe"),
                    GrantAccess::ReadExecute,
                    true
                ),
                (
                    format!(r"{EXE_DIR}\.cimp-offload.json"),
                    GrantAccess::ReadExecute,
                    true
                ),
                (
                    format!(r"{EXE_DIR}\.cimp-discovery"),
                    GrantAccess::ReadExecute,
                    false
                ),
                (format!(r"{HOME}\.gitconfig"), GrantAccess::ReadExecute, true),
                (
                    format!(r"{HOME}\.config\git"),
                    GrantAccess::ReadExecute,
                    false
                ),
                (format!(r"{HOME}\.claude"), GrantAccess::Full, false),
                (format!(r"{HOME}\.claude.json"), GrantAccess::Full, true),
                (
                    format!(r"{HOME}\.claude.json.backup"),
                    GrantAccess::Full,
                    true
                ),
                (
                    format!(r"{HOME}\.local\share\claude"),
                    GrantAccess::ReadExecute,
                    false
                ),
                (
                    format!(r"{HOME}\.local\state\claude"),
                    GrantAccess::Full,
                    false
                ),
            ]
        );
    }

    /// The OpenCode table, same treatment.
    #[test]
    fn the_opencode_grant_table_is_what_it_claims() {
        let rows = rows(opencode(), &[("USERPROFILE", HOME)]);
        let by_path: Vec<(String, GrantAccess)> = rows
            .iter()
            .map(|r| (r.path.to_string_lossy().replace('/', "\\"), r.access))
            .collect();
        assert_eq!(
            by_path,
            vec![
                // The same cImp three: the proxy child is unconditional in BOTH
                // harnesses since V37 Phase F.
                (format!(r"{EXE_DIR}\cimp.exe"), GrantAccess::ReadExecute),
                (
                    format!(r"{EXE_DIR}\.cimp-offload.json"),
                    GrantAccess::ReadExecute
                ),
                (
                    format!(r"{EXE_DIR}\.cimp-discovery"),
                    GrantAccess::ReadExecute
                ),
                (format!(r"{HOME}\.gitconfig"), GrantAccess::ReadExecute),
                (format!(r"{HOME}\.config\git"), GrantAccess::ReadExecute),
                (format!(r"{HOME}\.config\opencode"), GrantAccess::Full),
                (format!(r"{HOME}\.local\share\opencode"), GrantAccess::Full),
                (format!(r"{HOME}\.local\state\opencode"), GrantAccess::Full),
            ]
        );
    }

    /// **The credential dirs stay dark.** The one assertion in this file that
    /// is about the product's promise rather than its plumbing: a row for
    /// `~/.ssh`, for the home directory itself, or for cImp's own config would
    /// mean the boundary no longer says what the Settings screen says it says.
    #[test]
    fn no_grant_row_reaches_credentials_or_the_home_directory() {
        for harness in every_harness() {
            for p in paths(&rows(harness, &[("USERPROFILE", HOME)])) {
                let lower = p.to_ascii_lowercase();
                assert!(
                    !lower.contains(r"\.ssh"),
                    "{harness:?} grants an ssh path ({p}) — a tab that can read the user's \
                     private keys is not sandboxed"
                );
                assert!(
                    !lower.contains(r"\.aws") && !lower.contains(r"\.docker"),
                    "{harness:?} grants a credential directory ({p})"
                );
                // Component-EXACT, not a substring: `.cimp` is the
                // project-level config directory (`.cimp/config.json`, the file
                // that switches this sandbox off). `.cimp-offload.json` and
                // `.cimp-discovery` are different names and are the two the
                // proxy child legitimately reads — see `cimp_child_rows`, and
                // `the_proxy_child_gets_the_binary_and_its_discovery_and_nothing_wider`
                // for what stays dark beside them.
                assert!(
                    !lower.split('\\').any(|seg| seg == ".cimp"),
                    "{harness:?} grants cImp's own config ({p}) — the sandbox must not hand a \
                     model the file that switches the sandbox off"
                );
                // The home directory itself, exactly: a row FOR it (rather than
                // for something under it) is the accidental full-home grant.
                assert_ne!(
                    lower,
                    HOME.to_ascii_lowercase(),
                    "{harness:?} grants the whole home directory"
                );
            }
        }
    }

    /// Every row carries a reason, and the reason is a sentence rather than a
    /// placeholder — the property that makes this table a review artifact.
    #[test]
    fn every_grant_row_states_why() {
        for harness in every_harness() {
            for row in rows(harness, &[("USERPROFILE", HOME)]) {
                assert!(
                    row.reason.len() > 20,
                    "{:?} row {} has no real reason",
                    harness,
                    row.path.display()
                );
            }
        }
    }

    /// A relocated harness must be granted where it actually lives. Without the
    /// XDG lookups the boundary would sit around directories the tool never
    /// touches, and the tab would fail with denials pointing at paths the user
    /// deliberately moved away from.
    #[test]
    fn relocation_env_vars_move_the_grants() {
        let rows = rows(
            opencode(),
            &[
                ("USERPROFILE", HOME),
                ("XDG_CONFIG_HOME", r"D:\cfg"),
                ("XDG_DATA_HOME", r"D:\data"),
            ],
        );
        let p = paths(&rows);
        assert!(p.contains(&r"D:\cfg\opencode".to_string()), "{p:?}");
        assert!(p.contains(&r"D:\data\opencode".to_string()), "{p:?}");
        // …and the shared git row follows XDG_CONFIG_HOME too.
        assert!(p.contains(&r"D:\cfg\git".to_string()), "{p:?}");

        let rows = rows_claude_with_config_dir();
        assert!(
            paths(&rows).contains(&r"E:\claude-state".to_string()),
            "CLAUDE_CONFIG_DIR must move the Claude state grant"
        );
    }

    fn rows_claude_with_config_dir() -> Vec<GrantRow> {
        rows(
            claude(),
            &[("USERPROFILE", HOME), ("CLAUDE_CONFIG_DIR", r"E:\claude-state")],
        )
    }

    /// No home directory ⇒ no guesses about the HARNESS state. A table rooted
    /// at `\` would stamp ACEs on directories chosen by accident.
    ///
    /// cImp's own three survive, and must: they are anchored on
    /// `current_exe()`, not on a home that may not exist, and without them the
    /// tab's MCP child cannot start at all. With neither a home nor an
    /// executable there is nothing left to guess from, and the table is empty.
    #[test]
    fn a_missing_home_yields_no_home_anchored_rows() {
        for harness in every_harness() {
            assert_eq!(paths(&rows(harness, &[])), cimp_paths());
        }
        let e = env_of(&[]);
        assert!(grant_rows_with(claude(), &e, None).is_empty());
        assert!(grant_rows_with(opencode(), &e, None).is_empty());
    }

    /// **The proxy child's three, and nothing wider.** V37 Phase F made
    /// `cimp --offload-mcp` unconditional in every AI tab, so a sandboxed tab
    /// must be able to run cImp's binary and read its discovery data — and must
    /// NOT be able to read the rest of cImp's portable root, `settings.json`
    /// above all (backend URLs, API keys, every auth token the app holds).
    ///
    /// Pinned per harness because the child is unconditional in both, and
    /// pinned as "the exe DIRECTORY is not a row" because the one-character
    /// change from three file grants to one directory grant is the change that
    /// would quietly hand a compromised agent all of it.
    #[test]
    fn the_proxy_child_gets_the_binary_and_its_discovery_and_nothing_wider() {
        for harness in every_harness() {
            let rows = rows(harness, &[("USERPROFILE", HOME)]);
            let head: Vec<(String, GrantAccess, bool)> = rows
                .iter()
                .take(3)
                .map(|r| {
                    (
                        r.path.to_string_lossy().replace('/', "\\"),
                        r.access,
                        r.is_file,
                    )
                })
                .collect();
            assert_eq!(
                head,
                vec![
                    // Read+execute: the container has to LAUNCH this one.
                    (format!(r"{EXE_DIR}\cimp.exe"), GrantAccess::ReadExecute, true),
                    // Read-only, file-scoped, both of them.
                    (
                        format!(r"{EXE_DIR}\.cimp-offload.json"),
                        GrantAccess::ReadExecute,
                        true
                    ),
                    (
                        format!(r"{EXE_DIR}\.cimp-discovery"),
                        GrantAccess::ReadExecute,
                        false
                    ),
                ],
                "{harness:?}"
            );
            for p in paths(&rows) {
                let lower = p.to_ascii_lowercase();
                assert_ne!(
                    lower,
                    EXE_DIR.to_ascii_lowercase(),
                    "{harness:?} grants cImp's install DIRECTORY — settings.json and every \
                     token in it would come with it. The three rows above are file-scoped \
                     precisely so this row never exists"
                );
                for secret in ["settings.json", "tool-activity.jsonl"] {
                    assert!(
                        !lower.ends_with(secret),
                        "{harness:?} grants {secret} ({p}) — no secret cImp holds may reach a \
                         sandboxed child"
                    );
                }
                assert!(
                    !lower.split('\\').any(|seg| seg == "detection"),
                    "{harness:?} grants the detection store ({p})"
                );
            }
            // Nothing under cImp's root beyond those three, whatever else the
            // harness table grows.
            let under_root = paths(&rows)
                .into_iter()
                .filter(|p| {
                    p.to_ascii_lowercase()
                        .starts_with(&format!(r"{EXE_DIR}\").to_ascii_lowercase())
                })
                .collect::<Vec<_>>();
            assert_eq!(under_root, cimp_paths(), "{harness:?}");
        }
    }

    /// Nothing in the table is REQUIRED: harness state is created on first use,
    /// so a fresh machine legitimately has none of it, and failing preparation
    /// over an absent `~/.config/git` would refuse to sandbox a healthy tab.
    #[test]
    fn every_row_is_optional_so_a_fresh_machine_still_sandboxes() {
        for harness in every_harness() {
            for row in rows(harness, &[("USERPROFILE", HOME)]) {
                assert!(!row.required, "{} must be optional", row.path.display());
            }
        }
    }

    // ── the environment (decision B4) ──

    /// The override list is exactly the scratch redirection — and, the
    /// load-bearing half, does NOT contain `HOME`/`USERPROFILE`. Redirecting
    /// those (as `run_command` does) hands the user a logged-out tab with no
    /// history, which reads as a broken product rather than a confined one.
    #[test]
    fn only_the_scratch_is_redirected_and_home_is_left_alone() {
        let over = env_overrides(Path::new(r"S:\.cimp\sandbox-tmp\claude"));
        let names: Vec<&str> = over.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["TEMP", "TMP"]);
        for (k, _) in &over {
            assert_ne!(k, "HOME");
            assert_ne!(k, "USERPROFILE");
        }
        assert!(over
            .iter()
            .all(|(_, v)| v == &OsString::from(r"S:\.cimp\sandbox-tmp\claude")));
    }

    /// Path assertions are made on the SEGMENTS, not on a spelled-out string:
    /// a drive-letter literal is one component on Linux and the CI runner is
    /// Linux, so a `PathBuf::from(r"P:\proj\…")` expectation passes locally and
    /// fails there (V35's documented CI lesson).
    #[test]
    fn the_scratch_is_per_tab_inside_the_root() {
        let root = Path::new("proj");
        let segs = |id: &str| -> Vec<String> {
            scratch_dir(root, id)
                .strip_prefix(root)
                .expect("the scratch is always under the root")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(segs("claude"), vec![".cimp", "sandbox-tmp", "claude"]);
        // Two tabs never share a scratch.
        assert_ne!(scratch_dir(root, "claude"), scratch_dir(root, "claude-2"));
        // A tab id is user-editable text, not a path segment: it must never
        // introduce a separator or climb out of the scratch directory.
        assert_eq!(segs("../../etc"), vec![".cimp", "sandbox-tmp", ".._.._etc"]);
        assert_eq!(segs("///"), vec![".cimp", "sandbox-tmp", "___"]);
        assert_eq!(segs(""), vec![".cimp", "sandbox-tmp", "tab"]);
        assert_eq!(segs(".."), vec![".cimp", "sandbox-tmp", "tab"]);
        assert_eq!(segs(r"a\b"), vec![".cimp", "sandbox-tmp", "a_b"]);
    }

    // ── the two switches (decision B2) and the network rule (B3) ──

    #[test]
    fn tabs_are_sandboxed_only_when_both_switches_are_on() {
        let mut s = crate::settings::Settings::default();
        assert!(!tab_sandbox_cfg(&s).enabled, "default is off");
        s.sandbox.tabs = true;
        assert!(
            !tab_sandbox_cfg(&s).enabled,
            "`tabs` alone must not sandbox anything — it is a scope widener, not a master switch"
        );
        s.sandbox.tabs = false;
        s.sandbox.enabled = true;
        assert!(
            !tab_sandbox_cfg(&s).enabled,
            "the master switch alone must not reach tabs"
        );
        s.sandbox.tabs = true;
        assert!(tab_sandbox_cfg(&s).enabled);
    }

    /// B3, as a test rather than a comment: a sandboxed tab always has egress,
    /// whatever `allow_network` says, because the alternative is a bricked tab.
    #[test]
    fn a_sandboxed_tab_always_gets_the_network_capability() {
        let mut s = crate::settings::Settings::default();
        s.sandbox.enabled = true;
        s.sandbox.tabs = true;
        assert!(!s.sandbox.allow_network, "the tool-seam knob is still off");
        assert!(
            tab_sandbox_cfg(&s).allow_network,
            "a tab must get internetClient regardless of the run_command knob"
        );
        // …and the tool seams are unaffected by the tab switch.
        assert!(!SandboxCfg::from_settings(&s).allow_network);
    }

    /// The user-curated grant rows reach tabs too.
    #[test]
    fn extra_grant_dirs_carry_through_to_tabs() {
        let mut s = crate::settings::Settings::default();
        s.sandbox.enabled = true;
        s.sandbox.tabs = true;
        s.sandbox.extra_grant_dirs = vec![r"D:\tools".into(), "  ".into()];
        let cfg = tab_sandbox_cfg(&s);
        assert_eq!(cfg.extra_grant_dirs, vec![PathBuf::from(r"D:\tools")]);
    }

    /// Each of the four switch states produces a DIFFERENT explanation, and the
    /// sandboxed state produces none. A skip row that says "off" without saying
    /// which of two checkboxes is off is a row that sends the user hunting.
    #[test]
    fn the_skip_note_names_the_switch_that_is_off() {
        let mut s = crate::settings::Settings::default();
        let mut seen = std::collections::HashSet::new();
        for (enabled, tabs) in [(false, false), (false, true), (true, false)] {
            s.sandbox.enabled = enabled;
            s.sandbox.tabs = tabs;
            let note = off_note(&s);
            assert!(!note.is_empty(), "({enabled}, {tabs}) must explain itself");
            assert!(seen.insert(note), "two switch states share one note: {note}");
        }
        s.sandbox.enabled = true;
        s.sandbox.tabs = true;
        assert_eq!(off_note(&s), "", "a sandboxed tab has nothing to explain");
    }

    #[test]
    fn harness_labels_are_distinct() {
        assert_ne!(claude().label(), opencode().label());
    }

    // ── the setting itself: persistence and the TS mirror ──

    /// `sandbox.tabs` survives a save/load round trip, and a settings file
    /// written before Phase B loads as OFF rather than failing or defaulting to
    /// on. A security switch that a stale config file could turn ON would be a
    /// setting the user never chose.
    #[test]
    fn the_tabs_switch_round_trips_and_old_files_load_as_off() {
        let mut s = crate::settings::Settings::default();
        s.sandbox.enabled = true;
        s.sandbox.tabs = true;
        let json = serde_json::to_string(&s).expect("serialize");
        let back: crate::settings::Settings = serde_json::from_str(&json).expect("deserialize");
        assert!(back.sandbox.enabled && back.sandbox.tabs);

        // A pre-Phase-B `sandbox` object, with no `tabs` key at all.
        let old: crate::settings::SandboxSettings =
            serde_json::from_str(r#"{"enabled":true,"allow_network":true}"#).expect("old shape");
        assert!(old.enabled);
        assert!(
            !old.tabs,
            "a settings file written before this feature must not silently enable it"
        );
    }

    /// The hand-maintained TS mirror (`src/lib/settings/types.ts`), embedded at
    /// compile time so a Rust-side change that is not reflected there fails
    /// `cargo test` rather than showing up as a runtime shape mismatch in the
    /// Settings window. Same mechanism `checks::tests` uses for `CheckDef`.
    #[test]
    fn sandbox_settings_fields_are_mirrored_in_types_ts() {
        const TS_TYPES: &str = include_str!("../../../src/lib/settings/types.ts");
        // Destructured so ADDING a Rust field is a compile error here until the
        // author has decided what the mirror should say about it.
        let crate::settings::SandboxSettings {
            enabled: _,
            tabs: _,
            allow_network: _,
            extra_grant_dirs: _,
        } = crate::settings::SandboxSettings::default();
        let iface = TS_TYPES
            .split("export interface SandboxSettings {")
            .nth(1)
            .and_then(|s| s.split('}').next())
            .expect("types.ts declares SandboxSettings");
        for field in ["enabled", "tabs", "allow_network", "extra_grant_dirs"] {
            assert!(
                iface.contains(&format!("{field}:")),
                "`{field}` is missing from the `SandboxSettings` interface in \
                 src/lib/settings/types.ts — add it to keep the mirror in sync"
            );
        }
        // …and the defaults object the Settings window falls back to.
        let defaults = TS_TYPES
            .split("sandbox: {")
            .nth(1)
            .and_then(|s| s.split('}').next())
            .expect("types.ts carries a sandbox defaults block");
        assert!(
            defaults.contains("tabs: false"),
            "the TS defaults must ship `tabs: false` — the Rust default is off, and a mirror \
             that defaults it on would flip a security switch on any client that reads it"
        );
    }
}
