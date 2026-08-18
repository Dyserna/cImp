//! V33 contract C2 — **the environment an agent-spawned child is allowed to
//! see**, and the composition order every spawn seam builds its final
//! environment with.
//!
//! # Why this lives under `sandbox/` but is not gated by the sandbox
//!
//! The table below is the C2 *minimal environment*. It shipped with
//! `run_command` (Phase A) and stays UNCONDITIONAL there per milestone decision
//! 17: turning `sandbox.enabled` off removes the OS boundary and nothing else.
//! It is hoisted here because the V33 increment that sandboxes the `run_check`
//! and audit seams needs the same table — a sandboxed child is exactly the C2
//! threat class — and two copies of a security allowlist is one copy too many.
//!
//! What is *not* claimed: the plain (unsandboxed) `run_check` / audit spawns
//! keep their historical inherit-and-force environment. Those seams run
//! operator-authored commands, and narrowing their environment is a separate,
//! user-visible decision; the sandboxed path narrows it because the OS boundary
//! is already changing what that child can reach.
//!
//! [`ChildEnv`] is the composition half: base table → the seam's forced
//! variables → the sandbox's own redirections, last writer wins.

use std::ffi::OsString;

/// One environment variable an agent-spawned child is allowed to see, with the
/// reason it is granted.
pub struct EnvGrant {
    pub name: &'static str,
    /// Read by review and by `the_child_env_table_is_well_formed`, not by the
    /// spawn path — the reason is the point of the row, so it lives with the
    /// row rather than in a comment that can drift away from it.
    #[allow(dead_code)]
    pub why: &'static str,
}

/// The complete environment an agent-spawned child gets, built up from nothing.
///
/// **The two-sided bar this table has to clear (V33 spec decision 10).**
///
/// 1. *No secret cImp holds may reach the child.* cImp inherits the shell that
///    launched it, so its process environment routinely carries API keys
///    (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GITHUB_TOKEN`, …), CI
///    credentials and OAuth material, and it adds its own loopback bearer token
///    to the environments it composes. Until this table existed the child got
///    **all of it**: there was no `env_clear`, no `env_remove`, and the only
///    manipulation was the additive per-`CommandPolicy` grant. A model that got
///    one allowlisted program to print its own environment read every one of
///    those.
/// 2. *`git log`, a `cargo` probe and an `npm` probe must still work* (V33
///    live-verify item 7). That is why the toolchains' own state pointers are
///    here — `HOME`/`USERPROFILE`, `CARGO_HOME`, `RUSTUP_HOME`, the npm cache
///    and prefix, `PATH`, and the Windows plumbing (`SystemRoot`, `COMSPEC`,
///    `PATHEXT`) without which a Windows child cannot even load its DLLs.
///    Handing a tool a pointer to its OWN state directory is not a hole (spec
///    decision 3): the child runs as the same user and can read that directory
///    regardless. The win is everything NOT granted.
///
/// **Build up, never inherit-and-subtract.** A denylist of secret-shaped names
/// is a guess about naming, and the spec rejects it: the next key with an
/// unguessed name walks straight through. Everything absent from this table is
/// absent from the child, including names nobody has thought of yet.
///
/// **Deliberate omissions, so a later reader does not "fix" them:**
/// * `HTTP_PROXY`/`HTTPS_PROXY` — proxy URLs routinely embed credentials
///   (`http://user:pass@host`). A probe that needs the network through an
///   authenticating proxy is a `CommandPolicy` env grant, not a blanket one.
/// * `SSL_CERT_FILE`/`SSL_CERT_DIR`, `NODE_OPTIONS`, `NODE_PATH`,
///   `RUSTC_WRAPPER`, `RUSTFLAGS`, `CARGO_BUILD_*` — each names a file or flag
///   set the child would then load or execute. None is needed by a read-only
///   probe.
/// * `GIT_*` — not inheriting these is a security *gain*: an ambient
///   `GIT_EXEC_PATH`/`GIT_SSH_COMMAND` in cImp's environment used to reach the
///   child and could re-open exactly what the `git` `CommandPolicy` closes by
///   denying `--exec-path`. The policy's own `GIT_PAGER`/`GIT_CONFIG_NOSYSTEM`/
///   … are applied after this table and are unaffected.
/// * `USERNAME`/`USER`/`COMPUTERNAME` — identity, not state; no probe needs it.
pub const CHILD_ENV: &[EnvGrant] = &[
    // ── process plumbing ───────────────────────────────────────────────────
    EnvGrant {
        name: "PATH",
        why: "the child's OWN program resolution — git finding its libexec helpers, \
              cargo finding rustc, npm finding node. cImp resolves the top-level program \
              itself, but a stripped PATH breaks everything the child then runs.",
    },
    EnvGrant {
        name: "PATHEXT",
        why: "Windows: which extensions count as executable. Without it a child that \
              shells out cannot find `.cmd`/`.bat` shims — which is what `npm` is.",
    },
    EnvGrant {
        name: "COMSPEC",
        why: "Windows: the command processor used to run `.cmd`/`.bat` shims — and, for \
              the `run_check` seam, the program cImp itself spawns.",
    },
    EnvGrant {
        name: "SystemRoot",
        why: "Windows: system DLL loading (WinSock in particular). A Windows child with \
              no SystemRoot fails to start for reasons that look nothing like an env bug.",
    },
    EnvGrant {
        name: "SystemDrive",
        why: "Windows: the companion to SystemRoot; some toolchains build paths from it.",
    },
    EnvGrant {
        name: "windir",
        why: "Windows: the older spelling of SystemRoot, still read by parts of the CRT.",
    },
    EnvGrant {
        name: "TEMP",
        why: "Windows scratch directory — cargo and npm both write temp files.",
    },
    EnvGrant {
        name: "TMP",
        why: "Windows scratch directory (the other spelling).",
    },
    EnvGrant {
        name: "TMPDIR",
        why: "Unix scratch directory.",
    },
    EnvGrant {
        name: "NUMBER_OF_PROCESSORS",
        why: "Windows: job-count default for cargo/npm. Not sensitive; omitting it makes \
              probes serial on some tools.",
    },
    EnvGrant {
        name: "PROCESSOR_ARCHITECTURE",
        why: "Windows: how toolchains pick their native/arm64 shims.",
    },
    EnvGrant {
        name: "OS",
        why: "Windows: read by npm's shell shims to branch on platform.",
    },
    // ── per-user state directories ─────────────────────────────────────────
    EnvGrant {
        name: "HOME",
        why: "Unix home, and Git for Windows' preferred home. The tool's own config lives \
              here; the child can read it either way (same user), so this is a pointer, \
              not an escalation (spec decision 3).",
    },
    EnvGrant {
        name: "USERPROFILE",
        why: "Windows home — where `.gitconfig`, `.cargo` and `.npmrc` live.",
    },
    EnvGrant {
        name: "HOMEDRIVE",
        why: "Windows: Git for Windows composes HOME from HOMEDRIVE+HOMEPATH when HOME is \
              unset.",
    },
    EnvGrant {
        name: "HOMEPATH",
        why: "Windows: the other half of the HOMEDRIVE+HOMEPATH pair.",
    },
    EnvGrant {
        name: "APPDATA",
        why: "Windows: npm's global prefix (`%APPDATA%\\npm`) and several tools' config.",
    },
    EnvGrant {
        name: "LOCALAPPDATA",
        why: "Windows: npm's cache and cargo's fallback data dir.",
    },
    EnvGrant {
        name: "ProgramData",
        why: "Windows: machine-wide toolchain installs (a system-wide Node lives here).",
    },
    EnvGrant {
        name: "ProgramFiles",
        why: "Windows: where Git/Node are installed; tools compose absolute paths from it.",
    },
    EnvGrant {
        name: "ProgramFiles(x86)",
        why: "Windows: the 32-bit install root, for the same reason.",
    },
    EnvGrant {
        name: "XDG_CACHE_HOME",
        why: "Unix: npm/cargo cache location when the user moved it off ~/.cache.",
    },
    EnvGrant {
        name: "XDG_CONFIG_HOME",
        why: "Unix: config location when the user moved it off ~/.config.",
    },
    EnvGrant {
        name: "XDG_DATA_HOME",
        why: "Unix: data location when the user moved it off ~/.local/share.",
    },
    // ── toolchain state pointers (live-verify item 7) ──────────────────────
    EnvGrant {
        name: "CARGO_HOME",
        why: "Where the registry index, the crate cache and the cargo binaries live. A \
              `cargo` probe with the wrong CARGO_HOME re-downloads the world or fails \
              offline.",
    },
    EnvGrant {
        name: "RUSTUP_HOME",
        why: "Where the toolchains live. Without it the rustup shim cannot find rustc, so \
              every cargo probe fails.",
    },
    EnvGrant {
        name: "RUSTUP_TOOLCHAIN",
        why: "Which toolchain the shim selects; a project pinned by env rather than by \
              rust-toolchain.toml needs it to resolve the same way cImp does.",
    },
    EnvGrant {
        name: "npm_config_cache",
        why: "npm's cache directory (lowercase is npm's documented spelling).",
    },
    EnvGrant {
        name: "npm_config_prefix",
        why: "npm's global install prefix — where its own binaries resolve from.",
    },
    EnvGrant {
        name: "NPM_CONFIG_CACHE",
        why: "The uppercase spelling of the cache var, which npm also honors.",
    },
    EnvGrant {
        name: "NPM_CONFIG_PREFIX",
        why: "The uppercase spelling of the prefix var.",
    },
    // ── output shape ───────────────────────────────────────────────────────
    EnvGrant {
        name: "LANG",
        why: "Locale. Parsers downstream read the child's text; a missing locale silently \
              changes encoding on Unix.",
    },
    EnvGrant {
        name: "LC_ALL",
        why: "Locale override, same reason.",
    },
    EnvGrant {
        name: "LC_CTYPE",
        why: "Character-class locale, same reason.",
    },
    EnvGrant {
        name: "TZ",
        why: "Timezone — `git log` renders author dates with it, and a probe that reports \
              times in a different zone than the rest of the app is a support ticket.",
    },
];

/// Compose the child's environment from [`CHILD_ENV`], reading each name
/// through `lookup`. Names absent from cImp's own environment are simply not
/// set (the table is a *ceiling*, not a requirement list); a present-but-empty
/// value is passed through unchanged.
///
/// `lookup` is a parameter rather than a direct `std::env::var_os` call so the
/// tests can drive a synthetic environment without mutating the test process's
/// own (a process-wide `set_var` under a 32-thread suite is its own hazard).
pub fn minimal_env(lookup: &dyn Fn(&str) -> Option<OsString>) -> Vec<(&'static str, OsString)> {
    CHILD_ENV
        .iter()
        .filter_map(|g| lookup(g.name).map(|v| (g.name, v)))
        .collect()
}

/// The final environment block a *sandboxed* child gets, built in one place so
/// the three seams cannot disagree about the order.
///
/// # The order is the contract
///
/// 1. the C2 base ([`minimal_env`]) — the ceiling;
/// 2. the seam's forced variables (a `CommandPolicy`'s `env` for `run_command`,
///    `CheckDef::env` for `run_check`, the adapter's `env` for an audit tool);
/// 3. the sandbox's own redirections (`Prepared::env_overrides`) — TEMP/TMP/
///    HOME/USERPROFILE pointed at the mapped drive.
///
/// (3) must win over (1) and (2), because those names exist in the base and a
/// child that writes its scratch outside the sandbox's one writable place gets
/// denied. Nothing enforces the order but the caller, so every caller goes
/// through this type and [`the_sandboxed_environment_composes_base_then_seam_then_sandbox`]
/// pins the outcome.
#[derive(Debug, Default)]
pub struct ChildEnv {
    pairs: Vec<(OsString, OsString)>,
}

impl ChildEnv {
    /// Start from the C2 table.
    ///
    /// The base is taken VERBATIM — including the deliberate
    /// `npm_config_cache` / `NPM_CONFIG_CACHE` pair, which differs only in case
    /// (see [`CHILD_ENV`]). Only the overlays below replace case-insensitively.
    pub fn from_base(base: &[(&str, OsString)]) -> Self {
        Self {
            pairs: base
                .iter()
                .map(|(k, v)| (OsString::from(*k), v.clone()))
                .collect(),
        }
    }

    /// Set one variable, replacing any existing entry with the same name.
    ///
    /// Case-INSENSITIVE, because this composes a raw `CreateProcessW`
    /// environment block and Windows treats `Path` and `PATH` as one variable —
    /// emitting both would leave which one the child reads up to the loader.
    /// (`tokio::process::Command`, which the plain paths use, already does this
    /// for us on Windows; the hand-rolled block has to do it itself.)
    pub fn set(&mut self, name: &str, value: OsString) {
        let lower = name.to_ascii_lowercase();
        self.pairs
            .retain(|(k, _)| !k.to_string_lossy().to_ascii_lowercase().eq(&lower));
        self.pairs.push((OsString::from(name), value));
    }

    /// Apply a seam's forced variables, in order.
    pub fn overlay<K, V>(&mut self, vars: impl IntoIterator<Item = (K, V)>)
    where
        K: AsRef<str>,
        V: Into<OsString>,
    {
        for (k, v) in vars {
            self.set(k.as_ref(), v.into());
        }
    }

    /// The finished `(name, value)` list, in composition order.
    pub fn into_pairs(self) -> Vec<(OsString, OsString)> {
        self.pairs
    }

    /// One variable's current value — test/diagnostic accessor.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, name: &str) -> Option<&OsString> {
        let lower = name.to_ascii_lowercase();
        self.pairs
            .iter()
            .find(|(k, _)| k.to_string_lossy().to_ascii_lowercase() == lower)
            .map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_allowlisted(name: &str) -> bool {
        CHILD_ENV.iter().any(|g| g.name.eq_ignore_ascii_case(name))
    }

    /// The table itself: no duplicates, every row carries a reason, and the
    /// composition drops nothing and invents nothing.
    #[test]
    fn the_child_env_table_is_well_formed() {
        // Exact-name duplicates only. The `npm_config_*` / `NPM_CONFIG_*` pairs
        // differ ONLY in case and are deliberate: Unix lookups are
        // case-sensitive, so a user who exported the uppercase spelling would
        // be missed by a lowercase-only row (and vice versa). On Windows both
        // rows resolve to the same variable and the second `cmd.env` write is a
        // no-op with the same value.
        let mut seen: Vec<&str> = Vec::new();
        for grant in CHILD_ENV {
            assert!(
                !seen.contains(&grant.name),
                "`{}` is listed twice in CHILD_ENV",
                grant.name
            );
            seen.push(grant.name);
            assert!(
                grant.why.len() > 20,
                "`{}` is granted without a reason — the table is reviewed like the V32 \
                 class table, and an unreasoned row cannot be reviewed",
                grant.name
            );
            assert!(
                !grant.name.is_empty() && !grant.name.contains('='),
                "`{}` is not a usable variable name",
                grant.name
            );
        }
        assert!(
            CHILD_ENV.iter().any(|g| g.name == "PATH"),
            "dropping PATH from the table breaks every child; it must stay granted"
        );
        // The `run_check` seam spawns the shell itself, so the child needs the
        // name that tells it where the shell's own helpers live.
        assert!(
            CHILD_ENV.iter().any(|g| g.name == "COMSPEC"),
            "COMSPEC is the program the sandboxed `run_check` seam runs; it must stay granted"
        );

        // Composition: only allowlisted names come out, absent names are
        // skipped rather than set empty, and a name outside the table can never
        // be produced no matter what the lookup answers.
        let composed = minimal_env(&|k| match k {
            "PATH" => Some(OsString::from("/usr/bin")),
            "LANG" => Some(OsString::from("")),
            _ => None,
        });
        assert_eq!(composed.len(), 2, "only what the lookup answered: {composed:?}");
        assert!(composed.iter().all(|(k, _)| is_allowlisted(k)));
        assert!(composed.iter().any(|(k, v)| *k == "LANG" && v.is_empty()));
        // A lookup that answers EVERYTHING still yields exactly the table.
        let greedy = minimal_env(&|_| Some(OsString::from("x")));
        assert_eq!(
            greedy.len(),
            CHILD_ENV.len(),
            "the table is the ceiling; nothing outside it can be produced"
        );
    }

    /// **The composition order, as a test rather than a comment.**
    ///
    /// Base → seam → sandbox, last writer wins. The sandbox's TEMP/HOME
    /// redirection is the one that MUST survive: it points at the mapped drive,
    /// which is the only place a sandboxed child can write, and both of the
    /// earlier layers carry those same names.
    #[test]
    fn the_sandboxed_environment_composes_base_then_seam_then_sandbox() {
        let base = vec![
            ("PATH", OsString::from("C:/base/bin")),
            ("TEMP", OsString::from("C:/Users/me/AppData/Local/Temp")),
            ("HOME", OsString::from("C:/Users/me")),
            ("CARGO_HOME", OsString::from("C:/Users/me/.cargo")),
        ];
        let mut env = ChildEnv::from_base(&base);
        // (2) the seam's forced variables — a `CheckDef::env` / adapter env.
        env.overlay([
            ("CI".to_string(), "1".to_string()),
            // …which is allowed to override a base name.
            ("PATH".to_string(), "C:/seam/bin".to_string()),
            // …and to set its own scratch, which the sandbox then overrules.
            ("TEMP".to_string(), "C:/seam/tmp".to_string()),
        ]);
        // (3) the sandbox's redirections, LAST.
        env.overlay([
            ("TEMP".to_string(), OsString::from("S:\\")),
            ("HOME".to_string(), OsString::from("S:\\")),
        ]);

        assert_eq!(env.get("TEMP").unwrap(), &OsString::from("S:\\"),);
        assert_eq!(env.get("HOME").unwrap(), &OsString::from("S:\\"));
        assert_eq!(env.get("PATH").unwrap(), &OsString::from("C:/seam/bin"));
        assert_eq!(env.get("CI").unwrap(), &OsString::from("1"));
        // The base name nobody touched survives untouched.
        assert_eq!(
            env.get("CARGO_HOME").unwrap(),
            &OsString::from("C:/Users/me/.cargo")
        );
        // …and every name appears EXACTLY once: a raw environment block with
        // two `TEMP` entries leaves which one wins up to the loader.
        let pairs = env.into_pairs();
        let mut names: Vec<String> = pairs
            .iter()
            .map(|(k, _)| k.to_string_lossy().to_ascii_lowercase())
            .collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a name was emitted twice: {pairs:?}");
    }

    /// The overlay's case-insensitivity, spelled out: on Windows `Path` and
    /// `PATH` are one variable, so a seam that forces `Path` must REPLACE the
    /// base's `PATH` rather than sit beside it.
    #[test]
    fn an_overlay_replaces_a_base_name_whatever_its_case() {
        let base = vec![("PATH", OsString::from("C:/base"))];
        let mut env = ChildEnv::from_base(&base);
        env.overlay([("Path".to_string(), "C:/other".to_string())]);
        let pairs = env.into_pairs();
        assert_eq!(pairs.len(), 1, "the two spellings must collapse: {pairs:?}");
        assert_eq!(pairs[0].1, OsString::from("C:/other"));
    }

    /// …but the BASE keeps both spellings of the npm pair, because that pair is
    /// deliberate and only the overlays deduplicate.
    #[test]
    fn the_base_keeps_the_deliberate_npm_case_pair() {
        let base = minimal_env(&|_| Some(OsString::from("x")));
        let env = ChildEnv::from_base(&base);
        let pairs = env.into_pairs();
        assert_eq!(
            pairs.len(),
            CHILD_ENV.len(),
            "from_base must not collapse the table's deliberate case pairs"
        );
    }
}
