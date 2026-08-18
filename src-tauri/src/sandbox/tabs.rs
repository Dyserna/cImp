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

/// Which AI harness a tab runs. Chosen by `tabs::config::build_ai_tool_spec`
/// (the one place that already knows), carried on `PtyLaunchSpec`, and used here
/// for exactly one thing: picking the grant table.
///
/// Deliberately not `Other`: a tab whose command matches neither harness gets
/// `None` on the spec and is not sandboxed at all, because a grant table nobody
/// wrote is not a boundary — it is a tool that fails to start for reasons the
/// user cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// `claude` / `claude-local` — Claude Code.
    Claude,
    /// `opencode`.
    OpenCode,
}

impl Harness {
    /// The label used in row text.
    pub fn label(&self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::OpenCode => "OpenCode",
        }
    }
}

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
///
/// What a user adds themselves, through `sandbox.extra_grant_dirs`: package
/// manager caches (`~/.bun`, `%LOCALAPPDATA%\npm-cache`) if their harness
/// installs plugins at startup, and any toolchain the agent shells out to that
/// does not live under Program Files. Those surface first as a **denial row**,
/// which is the design — the lane names what the boundary refused, and the user
/// decides whether to widen it.
fn grant_rows_with(harness: Harness, env: &dyn Fn(&str) -> Option<OsString>) -> Vec<GrantRow> {
    let Some(home) = env("USERPROFILE").or_else(|| env("HOME")).map(PathBuf::from) else {
        // No home directory to anchor on: return nothing rather than guessing.
        // The tab still gets its project root and its program's install dir, and
        // whatever it then cannot read shows up as a denial row.
        return Vec::new();
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
    let mut rows = vec![
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
    ];

    match harness {
        Harness::Claude => {
            // `CLAUDE_CONFIG_DIR` relocates the state directory; honoring it
            // costs one lookup and its absence would silently confine a tab
            // away from the state it actually uses.
            let claude_dir = env("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude"));
            rows.extend([
                GrantRow {
                    path: claude_dir,
                    access: GrantAccess::Full,
                    is_file: false,
                    reason: "Claude Code's own state — projects, history, sessions, shell \
                             snapshots. Written on every turn; the CLI does not start without it",
                    required: false,
                },
                GrantRow {
                    path: home.join(".claude.json"),
                    access: GrantAccess::Full,
                    is_file: true,
                    reason: "Claude Code's top-level config, rewritten in place on most \
                             sessions. A FILE grant, so the home directory around it stays dark",
                    required: false,
                },
                GrantRow {
                    path: home.join(".claude.json.backup"),
                    access: GrantAccess::Full,
                    is_file: true,
                    reason: "the backup Claude Code rotates beside its config; same width, same \
                             file-only scope",
                    required: false,
                },
                GrantRow {
                    // `.local/bin/claude.exe` is a launcher — the install dir
                    // grant `prepare` derives from the program path covers only
                    // `bin`, and the JS payload lives in a sibling tree.
                    path: xdg("XDG_DATA_HOME", &[".local", "share"]).join("claude"),
                    access: GrantAccess::ReadExecute,
                    is_file: false,
                    reason: "the installed CLI payload (versions/<n>/…), which the launcher in \
                             ~/.local/bin executes. READ-ONLY on purpose: a sandboxed agent that \
                             can rewrite its own program image can persist across the boundary, \
                             so in-tab auto-update is refused rather than allowed",
                    required: false,
                },
                GrantRow {
                    path: xdg("XDG_STATE_HOME", &[".local", "state"]).join("claude"),
                    access: GrantAccess::Full,
                    is_file: false,
                    reason: "the CLI's lock/state directory (~/.local/state/claude)",
                    required: false,
                },
            ]);
        }
        Harness::OpenCode => {
            rows.extend([
                GrantRow {
                    path: xdg("XDG_CONFIG_HOME", &[".config"]).join("opencode"),
                    access: GrantAccess::Full,
                    is_file: false,
                    reason: "OpenCode's config tree — opencode.json(c), themes, and the \
                             `node_modules` it installs plugin dependencies into at startup, \
                             which is why this is read+WRITE",
                    required: false,
                },
                GrantRow {
                    path: xdg("XDG_DATA_HOME", &[".local", "share"]).join("opencode"),
                    access: GrantAccess::Full,
                    is_file: false,
                    reason: "OpenCode's data directory — auth.json, the session SQLite database \
                             (+ its -wal/-shm), logs, snapshots. Written continuously",
                    required: false,
                },
                GrantRow {
                    path: xdg("XDG_STATE_HOME", &[".local", "state"]).join("opencode"),
                    access: GrantAccess::Full,
                    is_file: false,
                    reason: "OpenCode's state directory (~/.local/state/opencode)",
                    required: false,
                },
            ]);
        }
    }
    rows
}

/// [`grant_rows_with`] against the real process environment.
pub fn grant_hints(harness: Harness) -> GrantHints {
    GrantHints {
        programs: Vec::new(),
        full_dirs: Vec::new(),
        rows: grant_rows_with(harness, &|k| std::env::var_os(k)),
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
/// carries it: the AppContainer engine is the only consumer today, and Landlock
/// (Phase D) will be the second.
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
        super::Plan::Plain(reason) => {
            super::record_skip_noting(&seam, &reason, tab_id, root, off_note(settings));
            TabPlan::Plain
        }
    }
}

#[cfg(test)]
mod tests {
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

    fn rows(harness: Harness, pairs: &[(&str, &str)]) -> Vec<GrantRow> {
        let e = env_of(pairs);
        grant_rows_with(harness, &e)
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
        let rows = rows(Harness::Claude, &[("USERPROFILE", HOME)]);
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
        let rows = rows(Harness::OpenCode, &[("USERPROFILE", HOME)]);
        let by_path: Vec<(String, GrantAccess)> = rows
            .iter()
            .map(|r| (r.path.to_string_lossy().replace('/', "\\"), r.access))
            .collect();
        assert_eq!(
            by_path,
            vec![
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
        for harness in [Harness::Claude, Harness::OpenCode] {
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
                assert!(
                    !lower.contains(r"\.cimp"),
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
        for harness in [Harness::Claude, Harness::OpenCode] {
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
            Harness::OpenCode,
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
            Harness::Claude,
            &[("USERPROFILE", HOME), ("CLAUDE_CONFIG_DIR", r"E:\claude-state")],
        )
    }

    /// No home directory ⇒ no guesses. An empty table is honest; a table rooted
    /// at `\` would stamp ACEs on directories chosen by accident.
    #[test]
    fn a_missing_home_yields_no_rows_rather_than_a_guess() {
        assert!(rows(Harness::Claude, &[]).is_empty());
        assert!(rows(Harness::OpenCode, &[]).is_empty());
    }

    /// Nothing in the table is REQUIRED: harness state is created on first use,
    /// so a fresh machine legitimately has none of it, and failing preparation
    /// over an absent `~/.config/git` would refuse to sandbox a healthy tab.
    #[test]
    fn every_row_is_optional_so_a_fresh_machine_still_sandboxes() {
        for harness in [Harness::Claude, Harness::OpenCode] {
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
        assert_ne!(Harness::Claude.label(), Harness::OpenCode.label());
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
