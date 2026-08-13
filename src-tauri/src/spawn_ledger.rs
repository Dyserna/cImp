//! V33 contract C1 — the external-process **spawn seam ledger**.
//!
//! The V33 spec opened by asserting cImp owns "two spawn seams". It owns
//! fourteen source sites across four different spawn mechanisms, and the
//! distinction that matters for sandboxing is not *how many* but *whose work
//! the child is doing*:
//!
//! * [`SpawnClass::AgentSpawn`] — the child's program/arguments are chosen (or
//!   selected from) something a model asked for. These are the seams a sandbox
//!   must eventually cover.
//! * [`SpawnClass::HostSpawn`] — cImp's own infrastructure. Sandboxing these
//!   would break the app: `workbench/git.rs` drives the shadow-checkpoint repo,
//!   and confining it breaks *restore*; `offload/supervisor.rs` runs the
//!   llama-server that answers the offload requests in the first place.
//!
//! The ledger is **data reviewed like the V32 tool-class table**, and it is
//! kept honest by the tripwire in this module's tests: an exhaustive scan of
//! every `.rs` file under `src/` must find exactly the sites listed here and no
//! others. Adding a spawn anywhere in the crate therefore fails the suite until
//! its row — and its reason — is written down.
//!
//! **Why this file allows dead code.** Nothing in the shipped binary reads
//! [`LEDGER`] yet; its consumers are the tripwire below and V33 stages 2–3,
//! which will confine the `AgentSpawn` rows. Same posture (and same reason) as
//! `offload::toolclass`'s `mutates_fs` accessor, which shipped in V32 behind an
//! `allow(dead_code)` naming V33 as its consumer — and whose `allow` V33 Phase F
//! then removed by landing that consumer, which is what this one is owed too.
#![allow(dead_code)]

/// Whose work the spawned child is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnClass {
    /// A model's request reached this spawn. The program, its arguments, or
    /// which of several configured commands runs is (at least partly) chosen by
    /// an agent — via the offload worker's tool dispatch, or over MCP from
    /// Claude/OpenCode. These are what V33 sandboxing is *for*.
    AgentSpawn,
    /// cImp's own infrastructure. The program and arguments are fixed in code
    /// or come from operator settings; no model input selects them. Confining
    /// these is out of scope and, for several rows, actively harmful.
    HostSpawn,
}

/// One external-process spawn site.
///
/// `file` is slash-separated and relative to `src-tauri/src/`. `symbol` names
/// the enclosing function rather than a line number, because line numbers rot
/// and a renamed function is a real review event. `count` is how many spawn
/// constructors the site's file contains **outside** `#[cfg(test)]` — platform
/// `cfg` arms all count, since the tripwire reads source text and must give the
/// same answer on every target.
pub struct SpawnSite {
    pub file: &'static str,
    pub symbol: &'static str,
    /// What actually ends up running.
    pub spawns: &'static str,
    pub class: SpawnClass,
    /// Spawn-constructor occurrences in `file` outside `#[cfg(test)]`.
    pub count: usize,
    /// Why it is classified this way — and, for `HostSpawn`, why sandboxing it
    /// would be wrong.
    pub reason: &'static str,
}

use SpawnClass::{AgentSpawn, HostSpawn};

/// Every external-process spawn in `src-tauri/src/`, classified.
///
/// Kept in file order. The tripwire
/// [`tests::the_spawn_ledger_is_exhaustive`] asserts this table matches the
/// tree exactly, so a new spawn cannot land unclassified.
pub const LEDGER: &[SpawnSite] = &[
    SpawnSite {
        file: "audit/mod.rs",
        symbol: "detect_tool",
        spawns: "<audit tool> --version (semgrep, gitleaks, …)",
        class: HostSpawn,
        count: 1,
        reason: "Capability detection for the Code Audit panel. The binary comes from the \
                 configured tool list or PATH and the only argument is a fixed `--version`; \
                 no model input reaches it. It runs on settings load and on a user click, \
                 not on a tool call.",
    },
    SpawnSite {
        file: "audit/runner.rs",
        symbol: "spawn_and_capture",
        spawns: "the resolved audit scanner binary (semgrep/gitleaks/…) with its argv",
        class: AgentSpawn,
        count: 1,
        reason: "Backs `security_audit`/`quality_audit`, which are reachable BOTH from the \
                 offload worker's dispatch and from Claude/OpenCode over the MCP proxy. The \
                 model chooses the root and which tools run; the scanners themselves then \
                 read the whole tree. Unlike `checks`, no shell is interposed — argv is \
                 built in code.",
    },
    SpawnSite {
        file: "checks/gitls.rs",
        symbol: "run_git",
        spawns: "git status/diff (async, for the changed-file filter)",
        class: HostSpawn,
        count: 1,
        reason: "Fixed read-only argv assembled in code to decide which files a check run \
                 should look at. No model input selects the arguments; the root is a \
                 configured project root.",
    },
    SpawnSite {
        file: "checks/mod.rs",
        symbol: "shell_command (spawned by spawn_capture)",
        spawns: "cmd.exe /C <check cmd>  |  sh -c <check cmd>",
        class: AgentSpawn,
        count: 2,
        reason: "Backs `run_check`, reachable from the offload worker AND from \
                 Claude/OpenCode over MCP. THE ONLY SEAM THAT RUNS THROUGH A SHELL. The \
                 command string is a `CheckDef::cmd` the operator authored — never \
                 model-supplied, which is why shell interpretation is intended here — but \
                 WHICH check runs, and in which configured root, is chosen by the caller. \
                 Two constructors, one per platform arm.",
    },
    SpawnSite {
        file: "graph/gitcmd.rs",
        symbol: "run_git",
        spawns: "git log/diff/ls-files (sync, for graph churn + impact metadata)",
        class: HostSpawn,
        count: 1,
        reason: "The code-graph indexer's own git reads. Fixed argv, driven by the rebuild \
                 and watcher threads, never by a tool call.",
    },
    SpawnSite {
        file: "ipc/commands.rs",
        symbol: "detection_open_rules_folder / content_open_folder",
        spawns: "explorer | open | xdg-open <fixed cImp directory>",
        class: HostSpawn,
        count: 6,
        reason: "Two \"reveal this folder\" IPC commands, each with a three-arm platform \
                 `cfg!`. The path is computed by cImp (the detection rules dir, the content \
                 log dir) and cannot be supplied by the caller; the trigger is a button in \
                 the app's own UI.",
    },
    SpawnSite {
        file: "offload/mcp_host.rs",
        symbol: "connect_stdio",
        spawns: "a configured third-party stdio MCP server binary",
        class: HostSpawn,
        count: 1,
        reason: "AGENT-SERVING, NOT AGENT WORK — classified explicitly because it is the \
                 easiest row to mis-file. The binary, argv and env come from \
                 `McpServerConfig` (operator settings); the connection is established when \
                 the host connects, not when a model calls a tool. What the model reaches \
                 is the already-running server's tools over JSON-RPC, which the tool-class \
                 gate covers. Sandboxing the server process would break the servers the \
                 user deliberately configured.",
    },
    SpawnSite {
        file: "offload/supervisor.rs",
        symbol: "spawn_child",
        spawns: "llama-server",
        class: HostSpawn,
        count: 1,
        reason: "The local inference server itself. MUST NEVER BE SANDBOXED: it needs the \
                 GPU, the model files, and a listening socket — confining it disables the \
                 offload feature entirely. Program and argv come from `ServerCommand` in \
                 settings.",
    },
    SpawnSite {
        file: "offload/tools/run_command.rs",
        symbol: "execute",
        spawns: "an allowlisted, bare-named program resolved through PATH",
        class: AgentSpawn,
        count: 1,
        reason: "The archetypal agent spawn: the model names the program and every \
                 argument. Already the most constrained seam (deny-by-default allowlist, \
                 bare-name-only so PATH decides the binary, per-program `CommandPolicy` \
                 flag/subcommand rules, 120 s cap, output cap) and, since V33 contract C2, \
                 the only one with a minimal environment.",
    },
    SpawnSite {
        file: "preview/mod.rs",
        symbol: "open_external",
        spawns: "the OS default handler for a URL (browser), via tauri-plugin-opener",
        class: HostSpawn,
        count: 1,
        reason: "NOT a `Command::new` — the `open` crate behind `tauri-plugin-opener` does \
                 the spawning, which is why the ledger's needle set has to cover more than \
                 one mechanism. Fires when a user clicks an external link in a Preview tab. \
                 The screen is `preview::is_externally_openable` (scheme allowlist: http/https \
                 only, the V14 Follina-class fix) and NOT `is_allowed_preview_host`, which \
                 gates the embedded webview's navigation — this call is that check's REJECT \
                 path. The IPC twin is inert: `capabilities/default.json` grants \
                 `opener:allow-open-url`, which enables the command with NO url scope, and \
                 the plugin's own `is_url_allowed` is `allowed.any(..)` over an empty list — \
                 every frontend `plugin:opener|open_url` is refused `ForbiddenUrl`. Enforced \
                 by `the_opener_grant_stays_scopeless`, because that is a property of a JSON \
                 file this scan cannot see.",
    },
    SpawnSite {
        file: "procutil.rs",
        symbol: "kill_tree",
        spawns: "taskkill /T /F /PID <pid of a child cImp already owns>",
        class: HostSpawn,
        count: 1,
        reason: "The process-tree reaper. It is the mechanism that ENFORCES the timeout/cancel \
                 contract on the agent seams; sandboxing it would defeat the containment it \
                 provides. Its only argument is a pid cImp holds. The spawn counted here is the \
                 Windows arm; the Unix arm signals a process group directly (`killpg`, V33 C3) \
                 and spawns nothing.",
    },
    SpawnSite {
        file: "pty/manager.rs",
        symbol: "PtyManager::start",
        spawns: "the AI tab's harness binary (claude / opencode / a shell) under a PTY",
        class: AgentSpawn,
        count: 1,
        reason: "Every AI tab. The program and args come from the tab's configured \
                 `PtyLaunchSpec`, but the process it starts IS the agent, and everything \
                 that agent later runs is its child — so this is the widest agent seam by \
                 far. It is the one seam that does not go through `tokio::process`, which is \
                 why it needed its own job-object entry point: `process_guard::guard_pid` \
                 (V33 contract C3) takes the pid portable-pty reports, since `guard_child` \
                 is typed to a `tokio::process::Child`. Before that it was the only cImp \
                 child OUTSIDE the kill-on-job-close job. Its env discipline is \
                 `env_remove`-based and is deliberately NOT the C2 allowlist: a harness \
                 needs the user's full interactive environment.",
    },
    SpawnSite {
        file: "tabs/config.rs",
        symbol: "note_opencode_version",
        spawns: "opencode --version",
        class: HostSpawn,
        count: 1,
        reason: "A one-shot version probe run once per tab spawn to feed the harness-version \
                 tripwire. Fixed single argument; best-effort in every direction. The env \
                 composed a few lines away in this same file is the TAB spawn's env, which \
                 keeps its `env_remove` discipline (C2 applies to `run_command` only).",
    },
    SpawnSite {
        file: "workbench/git.rs",
        symbol: "run_inner",
        spawns: "git, against the project root or the shadow-checkpoint repo",
        class: HostSpawn,
        count: 1,
        reason: "MUST NEVER BE SANDBOXED. This is the shadow-checkpoint engine: it writes \
                 the checkpoint commits and, on restore, writes the user's working tree back \
                 out. A confined filesystem view here does not merely degrade a feature — it \
                 breaks RESTORE, i.e. the one path a user reaches for after an agent did \
                 something wrong. Argv is always built in code; `GIT_DIR`/`GIT_WORK_TREE` \
                 are set explicitly per call.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    // ── the scanner ────────────────────────────────────────────────────────
    //
    // Everything below reads Rust source as TEXT. Two properties make that
    // trustworthy enough to gate a security ledger on:
    //
    //  1. comments, strings and char literals are blanked before any needle is
    //     looked for, and the blanking is self-checked (`code_only` must leave
    //     no `"` behind — a desynced lexer always does);
    //  2. every helper here has a positive control in
    //     `the_scanner_finds_what_it_claims_to_find`, INCLUDING a control that
    //     the ledger comparison rejects an empty parse. A tripwire that passes
    //     when it read nothing is the failure mode this whole design is built
    //     against.

    /// The spawn constructors this crate can reach. Assembled with `concat!` so
    /// this file does not trip its own scan — the house style from
    /// `offload/agent.rs`'s admission-rule tripwire.
    ///
    /// Four mechanisms, not one: `Command::new` covers `std::process`,
    /// `tokio::process` and the `StdCommand`-style aliases (the needle is a
    /// suffix of all of them); `spawn_command` is portable-pty's; `open_url` /
    /// `open_path` are tauri-plugin-opener handing a target to the OS shell.
    /// `open_path` has no call site today and is listed so that adding one
    /// fails this test.
    const SPAWN_NEEDLES: &[&str] = &[
        concat!("Command::", "new"),
        concat!("spawn_", "command"),
        concat!("open_", "url"),
        concat!("open_", "path"),
    ];

    fn utf8_len(b: u8) -> usize {
        if b < 0x80 {
            1
        } else if b >> 5 == 0b110 {
            2
        } else if b >> 4 == 0b1110 {
            3
        } else {
            4
        }
    }

    /// Where a char literal starting at `i` ends, or `None` when the quote is a
    /// lifetime/label (`&'a str`, `'outer: loop`) rather than a literal.
    fn char_lit_end(b: &[u8], i: usize) -> Option<usize> {
        if b.get(i + 1) == Some(&b'\\') {
            let mut j = i + 3;
            match b.get(i + 2) {
                Some(&b'u') => {
                    j = i + 4;
                    while j < b.len() && b[j] != b'}' {
                        j += 1;
                    }
                    j += 1;
                }
                Some(&b'x') => j = i + 5,
                _ => {}
            }
            return if b.get(j) == Some(&b'\'') {
                Some(j)
            } else {
                None
            };
        }
        let start = i + 1;
        let end = start + utf8_len(*b.get(start)?);
        if b.get(end) == Some(&b'\'') {
            Some(end)
        } else {
            None
        }
    }

    /// If a raw string starts at `i` (`r"`, `r#"`, `br##"` …), the index of its
    /// opening quote and the hash count. Requires a token boundary before `i`
    /// so the `r` inside an identifier is not mistaken for a prefix.
    fn raw_string_start(b: &[u8], i: usize) -> Option<(usize, usize)> {
        if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
            return None;
        }
        let mut j = i;
        if b.get(j) == Some(&b'b') {
            j += 1;
        }
        if b.get(j) != Some(&b'r') {
            return None;
        }
        j += 1;
        let hash_start = j;
        while b.get(j) == Some(&b'#') {
            j += 1;
        }
        if b.get(j) != Some(&b'"') {
            return None;
        }
        Some((j, j - hash_start))
    }

    /// Replace every byte inside a comment, string or char literal with a
    /// space, keeping newlines and byte offsets intact so line numbers and
    /// `#[cfg(test)]` spans still line up. The result is CODE ONLY.
    ///
    /// Self-checked by the caller: valid Rust code, once its literals are
    /// blanked, contains no `"` at all. Any survivor means the lexer lost sync
    /// and the scan below cannot be trusted.
    fn code_only(src: &str) -> String {
        let b = src.as_bytes();
        let n = b.len();
        let mut out = b.to_vec();
        let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
            for byte in out.iter_mut().take(to.min(n)).skip(from) {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        };
        let mut i = 0usize;
        while i < n {
            if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
                let start = i;
                while i < n && b[i] != b'\n' {
                    i += 1;
                }
                blank(&mut out, start, i);
            } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                let start = i;
                let mut depth = 1usize;
                i += 2;
                while i < n && depth > 0 {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                blank(&mut out, start, i);
            } else if let Some((quote, hashes)) = raw_string_start(b, i) {
                let start = i;
                let mut j = quote + 1;
                loop {
                    if j >= n {
                        break;
                    }
                    if b[j] == b'"' {
                        let mut k = j + 1;
                        let mut seen = 0usize;
                        while seen < hashes && b.get(k) == Some(&b'#') {
                            k += 1;
                            seen += 1;
                        }
                        if seen == hashes {
                            j = k;
                            break;
                        }
                    }
                    j += 1;
                }
                i = j;
                blank(&mut out, start, i);
            } else if b[i] == b'"' {
                let start = i;
                let mut j = i + 1;
                while j < n {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                i = j;
                blank(&mut out, start, i);
            } else if b[i] == b'\'' {
                match char_lit_end(b, i) {
                    Some(end) => {
                        blank(&mut out, i, end + 1);
                        i = end + 1;
                    }
                    None => i += 1,
                }
            } else {
                i += 1;
            }
        }
        String::from_utf8(out).expect("blanking only ever writes ASCII spaces")
    }

    /// `code_only` plus its self-check, so every caller gets the guarantee.
    fn code_of(rel: &str, src: &str) -> String {
        let code = code_only(src);
        assert_eq!(
            code.len(),
            src.len(),
            "{rel}: blanking changed the byte length — offsets would be wrong"
        );
        assert!(
            !code.contains('"'),
            "{rel}: a double quote survived literal-blanking, so the scanner's lexer lost \
             sync and its needle counts cannot be trusted. Fix `code_only` before trusting \
             this ledger."
        );
        code
    }

    /// Does a `cfg(...)` predicate select a TEST build? Broadened past the bare
    /// `cfg(test)` to the `all(...)`/`any(...)` combinator forms, the same
    /// broadening `graph::builder` documents for its own test detection —
    /// while excluding `not(test)`, which `offload/outbound.rs` really uses and
    /// which means the exact opposite.
    fn selects_test(pred: &str) -> bool {
        let compact: String = pred.chars().filter(|c| !c.is_whitespace()).collect();
        let compact = compact.replace("not(test)", "");
        compact
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|t| t == "test")
    }

    /// End of the braced block that starts at `open`, one past its `}`.
    fn skip_block(b: &[u8], open: usize) -> usize {
        let mut depth = 0usize;
        let mut i = open;
        while i < b.len() {
            match b[i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return i + 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        b.len()
    }

    /// End of the item that follows an attribute ending at `from`. An item is
    /// either brace-bodied (`mod`, `fn`, `impl`, `struct`) or `;`-terminated
    /// (`use`, `const`). Parens/brackets are tracked so the `;` in `[u8; 4]`
    /// does not end the item early.
    fn item_end(b: &[u8], from: usize) -> usize {
        let mut i = from;
        let (mut paren, mut brack) = (0i32, 0i32);
        while i < b.len() {
            match b[i] {
                b'(' => paren += 1,
                b')' => paren -= 1,
                b'[' => brack += 1,
                b']' => brack -= 1,
                b';' if paren == 0 && brack == 0 => return i + 1,
                b'{' if paren == 0 && brack == 0 => return skip_block(b, i),
                _ => {}
            }
            i += 1;
        }
        b.len()
    }

    /// Byte ranges of every `#[cfg(<test-selecting>)]` item in blanked code.
    fn test_regions(code: &str) -> Vec<(usize, usize)> {
        let b = code.as_bytes();
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut search = 0usize;
        while let Some(rel) = code[search..].find("#[cfg(") {
            let at = search + rel;
            // Balanced-paren scan over the predicate.
            let mut i = at + "#[cfg".len();
            let mut depth = 0i32;
            let start = i + 1;
            while i < b.len() {
                match b[i] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let pred = &code[start.min(b.len())..i.min(b.len())];
            // Past the predicate's `)` and the attribute's `]`.
            let mut after = i + 1;
            while after < b.len() && b[after] != b']' {
                after += 1;
            }
            after += 1;
            if selects_test(pred) {
                let end = item_end(b, after);
                out.push((at, end));
            }
            search = after.min(b.len()).max(at + 1);
        }
        out
    }

    /// One spawn-constructor occurrence.
    struct Hit {
        line: usize,
        needle: &'static str,
        in_test: bool,
    }

    fn scan(rel: &str, src: &str) -> Vec<Hit> {
        let code = code_of(rel, src);
        let regions = test_regions(&code);
        let mut hits = Vec::new();
        for needle in SPAWN_NEEDLES {
            let mut from = 0usize;
            while let Some(off) = code[from..].find(needle) {
                let at = from + off;
                hits.push(Hit {
                    line: code[..at].matches('\n').count() + 1,
                    needle,
                    in_test: regions.iter().any(|(s, e)| at >= *s && at < *e),
                });
                from = at + needle.len();
            }
        }
        hits
    }

    fn production_hits(rel: &str, src: &str) -> Vec<Hit> {
        scan(rel, src).into_iter().filter(|h| !h.in_test).collect()
    }

    // ── the ledger comparison ──────────────────────────────────────────────

    /// The ledger's expected production-spawn count per file.
    fn ledger_counts() -> BTreeMap<&'static str, usize> {
        let mut m: BTreeMap<&'static str, usize> = BTreeMap::new();
        for site in LEDGER {
            *m.entry(site.file).or_default() += site.count;
        }
        m
    }

    /// Compare a set of `(relative path, source)` pairs against a ledger.
    /// `Err` lists every discrepancy. Extracted so the controls below can feed
    /// it known-bad input — a tripwire whose failure path is never exercised is
    /// an assumption, not a test.
    fn audit_against(
        files: &[(String, String)],
        expected: &BTreeMap<&'static str, usize>,
    ) -> Result<usize, Vec<String>> {
        let mut problems = Vec::new();
        if files.is_empty() {
            return Err(vec![
                "the scan received NO source files — an empty parse must fail, never pass"
                    .to_string(),
            ]);
        }
        let mut found: BTreeMap<String, Vec<Hit>> = BTreeMap::new();
        for (rel, src) in files {
            let hits = production_hits(rel, src);
            if !hits.is_empty() {
                found.insert(rel.clone(), hits);
            }
        }
        let mut total = 0usize;
        for (rel, hits) in &found {
            total += hits.len();
            match expected.get(rel.as_str()) {
                Some(&want) if want == hits.len() => {}
                Some(&want) => problems.push(format!(
                    "{rel}: ledger says {want} spawn(s), the tree has {} (lines {:?})",
                    hits.len(),
                    hits.iter().map(|h| h.line).collect::<Vec<_>>()
                )),
                None => problems.push(format!(
                    "{rel}: spawns an external process at line(s) {:?} ({}) but has NO row in \
                     `spawn_ledger::LEDGER` — classify it AgentSpawn or HostSpawn and record \
                     the reason (V33 contract C1)",
                    hits.iter().map(|h| h.line).collect::<Vec<_>>(),
                    hits.iter().map(|h| h.needle).collect::<Vec<_>>().join(", ")
                )),
            }
        }
        for file in expected.keys() {
            if !found.contains_key(*file) {
                problems.push(format!(
                    "{file}: has a `spawn_ledger::LEDGER` row but the tree shows no spawn there \
                     — the seam moved or was removed; update the ledger"
                ));
            }
        }
        if problems.is_empty() {
            Ok(total)
        } else {
            Err(problems)
        }
    }

    // ── the sources ────────────────────────────────────────────────────────

    /// Every ledger'd file's text, pinned at COMPILE time. This is the
    /// `include_str!` half of the tripwire (`offload/agent.rs`'s multi-file
    /// variant): it cannot be defeated by a stale checkout or a wrong cwd.
    fn ledger_sources() -> Vec<(String, String)> {
        [
            ("audit/mod.rs", include_str!("audit/mod.rs")),
            ("audit/runner.rs", include_str!("audit/runner.rs")),
            ("checks/gitls.rs", include_str!("checks/gitls.rs")),
            ("checks/mod.rs", include_str!("checks/mod.rs")),
            ("graph/gitcmd.rs", include_str!("graph/gitcmd.rs")),
            ("ipc/commands.rs", include_str!("ipc/commands.rs")),
            ("offload/mcp_host.rs", include_str!("offload/mcp_host.rs")),
            (
                "offload/supervisor.rs",
                include_str!("offload/supervisor.rs"),
            ),
            (
                "offload/tools/run_command.rs",
                include_str!("offload/tools/run_command.rs"),
            ),
            ("preview/mod.rs", include_str!("preview/mod.rs")),
            ("procutil.rs", include_str!("procutil.rs")),
            ("pty/manager.rs", include_str!("pty/manager.rs")),
            ("tabs/config.rs", include_str!("tabs/config.rs")),
            ("workbench/git.rs", include_str!("workbench/git.rs")),
        ]
        .into_iter()
        .map(|(rel, src)| (rel.to_string(), src.to_string()))
        .collect()
    }

    /// Every `.rs` file under `src/`, as `(slash path, contents)` — the
    /// EXHAUSTIVE half. `include_str!` can only name files someone remembered;
    /// a brand-new module that spawns something is exactly what nobody
    /// remembers, so the ledger's completeness claim has to come from a walk.
    /// Resolved from the manifest, not the cwd, so it answers the same from any
    /// working directory (the `graph::index::notes` pattern).
    fn source_files() -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, root, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p)
                        .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, text));
                }
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        assert!(
            out.len() > 100,
            "the source walk found only {} files — a broken walk finds no unledgered spawn \
             and passes, which is the one outcome this tripwire may never have",
            out.len()
        );
        out
    }

    // ── the tripwire ───────────────────────────────────────────────────────

    /// V33 C1 — every external-process spawn in the crate is in [`LEDGER`],
    /// and [`LEDGER`] names nothing that is not one.
    ///
    /// Run twice over two independent views of the tree: the compile-time
    /// `include_str!` copies of the ledger'd files (which proves the counts on
    /// the rows themselves) and a filesystem walk of ALL of `src/` (which
    /// proves no other file spawns anything). The V33 spec's "two spawn seams"
    /// is what an unverified list looks like.
    #[test]
    fn the_spawn_ledger_is_exhaustive() {
        let expected = ledger_counts();
        assert!(!expected.is_empty(), "the ledger itself is empty");

        // 1. compile-time view — the ledger'd files only.
        match audit_against(&ledger_sources(), &expected) {
            Ok(n) => assert!(n > 0, "the include_str! pass counted zero spawns"),
            Err(problems) => panic!(
                "spawn ledger mismatch against the compiled-in sources:\n  {}",
                problems.join("\n  ")
            ),
        }

        // 2. filesystem view — the WHOLE tree.
        let all = source_files();
        match audit_against(&all, &expected) {
            Ok(n) => assert!(n > 0, "the tree walk counted zero spawns"),
            Err(problems) => panic!(
                "spawn ledger mismatch against the {} source files under src/:\n  {}",
                all.len(),
                problems.join("\n  ")
            ),
        }
    }

    /// Every agent-reachable seam is still named, with its reason. The counts
    /// above catch a spawn that MOVES; this catches a row being quietly
    /// downgraded to `HostSpawn` to make a future sandbox check pass.
    #[test]
    fn the_four_agent_seams_are_still_classified_as_agent_seams() {
        for file in [
            "audit/runner.rs",
            "checks/mod.rs",
            "offload/tools/run_command.rs",
            "pty/manager.rs",
        ] {
            let site = LEDGER
                .iter()
                .find(|s| s.file == file)
                .unwrap_or_else(|| panic!("{file} lost its ledger row"));
            assert_eq!(
                site.class,
                AgentSpawn,
                "{file} was reclassified away from AgentSpawn — it is reachable from a model \
                 (V33 C1); if that is genuinely no longer true, the reason field has to say so"
            );
        }
        // …and the rows whose sandboxing would break the app stay HostSpawn.
        for file in [
            "workbench/git.rs",
            "offload/supervisor.rs",
            "graph/gitcmd.rs",
            "checks/gitls.rs",
            "audit/mod.rs",
            "tabs/config.rs",
            "procutil.rs",
            "ipc/commands.rs",
            "offload/mcp_host.rs",
            "preview/mod.rs",
        ] {
            let site = LEDGER
                .iter()
                .find(|s| s.file == file)
                .unwrap_or_else(|| panic!("{file} lost its ledger row"));
            assert_eq!(site.class, HostSpawn, "{file} must stay a host spawn");
        }
        for site in LEDGER {
            assert!(
                site.reason.len() > 60,
                "{}: a ledger row without a real reason is a list, not a ledger",
                site.file
            );
        }
    }

    /// **The one spawn seam whose reachability is decided outside `src/`.**
    ///
    /// `preview/mod.rs`'s row spawns through `tauri-plugin-opener`, and that
    /// plugin also exposes `open_url` to the WEBVIEW over IPC. Whether the
    /// frontend can reach it is settled entirely by `capabilities/default.json`
    /// — a file no source scan in this module reads — so the ledger's claim
    /// about it is asserted here rather than believed.
    ///
    /// **What makes it safe today, measured against the plugin's source**
    /// (`tauri-plugin-opener-2.5.4`):
    ///
    /// * `commands::open_url` runs `scope.is_url_allowed(&url, with)` before
    ///   `app.opener().open_url(..)`, and `Scope::is_url_allowed` is
    ///   `self.allowed.iter().any(..)` — **an empty allow list denies
    ///   everything**, it does not mean "unrestricted".
    /// * The granted permission is `allow-open-url`, whose own description is
    ///   "Enables the open_url command **without any pre-configured scope**".
    ///   It carries no `scope` key, and the crate ships no plugin `global_scope`
    ///   (both confirmed in `gen/schemas/acl-manifests.json`). So the allow list
    ///   really is empty and every IPC call returns `ForbiddenUrl`.
    /// * The URL scope lives in the SEPARATE `allow-default-urls` permission
    ///   (`http://*`, `https://*`, `mailto:*`, `tel:*`), which the `opener:default`
    ///   set pulls in. cImp grants neither.
    ///
    /// **Which is why this is a test and not a comment.** The safety is a
    /// property of what is ABSENT from a JSON file, and the single most likely
    /// future edit — swapping in `opener:default` to make the two
    /// `target="_blank"` links in `SettingsApp.svelte` work again (the plugin's
    /// injected click handler `preventDefault`s them and then invokes the
    /// forbidden command, so they currently do nothing) — would hand the
    /// webview an unscreened OS opener as a side effect, and would widen it past
    /// what `preview::is_externally_openable` allows on the Rust side, since the
    /// default scope includes `mailto:`/`tel:`. That is a decision worth taking
    /// deliberately; this test makes it impossible to take by accident.
    #[test]
    fn the_opener_grant_stays_scopeless() {
        // The two capability files, read as text: a `scope` on the grant is what
        // makes it reachable, and it can only arrive by editing one of them.
        for (name, json) in [
            ("default.json", include_str!("../capabilities/default.json")),
            ("main.json", include_str!("../capabilities/main.json")),
        ] {
            let cap: serde_json::Value =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{name}: {e}"));
            let perms = cap["permissions"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} has no permissions array"));
            for p in perms {
                // A scope-bearing grant is an object (`{"identifier": .., "allow": [..]}`);
                // a plain grant is a string. Any object naming the opener is a
                // scope, whatever its shape.
                if let Some(s) = p.as_str() {
                    assert!(
                        !matches!(s, "opener:default" | "opener:allow-default-urls"),
                        "{name} grants {s}, which carries the http/https/mailto/tel URL scope: \
                         the webview can now reach the OS opener over IPC, bypassing \
                         preview::is_externally_openable. Re-read this test's doc before \
                         deciding that is what you want, and update preview/mod.rs's ledger row."
                    );
                } else {
                    assert!(
                        !p.to_string().contains("opener"),
                        "{name} gives the opener plugin an inline scope ({p}) — see this test's doc"
                    );
                }
            }
        }
        // …and the grant the ledger row names is still the scope-less one, so
        // "the frontend cannot reach it" keeps meaning what the row says.
        let default_json = include_str!("../capabilities/default.json");
        assert!(
            default_json.contains("opener:allow-open-url"),
            "preview/mod.rs's ledger row describes a grant that is no longer there"
        );
    }

    // ── positive controls ──────────────────────────────────────────────────

    /// The controls. Each one feeds the scanner input whose answer is known,
    /// including input it must REJECT — the V32 lesson that a tripwire nobody
    /// has seen fail is an assumption.
    ///
    /// The fixtures below contain the literal needles, but only inside string
    /// literals inside a `#[cfg(test)]` module, so the tree scan blanks them
    /// twice over and this file stays out of its own ledger.
    #[test]
    fn the_scanner_finds_what_it_claims_to_find() {
        // 1. a plain production spawn is found.
        let hits = production_hits("f.rs", "fn f() { let c = Command::new(\"evil\"); }\n");
        assert_eq!(hits.len(), 1, "a bare production spawn must be found");
        assert_eq!(hits[0].line, 1);

        // 2. a mention in a comment is NOT a spawn (pty/manager.rs really has
        //    one of these, and counting it would have made the ledger wrong).
        assert!(
            production_hits("f.rs", "// same as the Command::new above\nfn f() {}\n").is_empty(),
            "a comment mention must not count"
        );
        assert!(
            production_hits("f.rs", "/// [`x`]: tokio::process::Command::new\nfn f() {}\n")
                .is_empty(),
            "a doc-link mention must not count"
        );

        // 3. a needle inside a string literal is not code.
        assert!(
            production_hits("f.rs", "fn f() { let s = \"Command::new\"; }\n").is_empty(),
            "a string literal must not count"
        );
        assert!(
            production_hits("f.rs", "fn f() { let s = r#\"Command::new\"#; }\n").is_empty(),
            "a raw string literal must not count"
        );

        // 4. ~15 files carry test-only `Command::new(\"git\")`; drowning in
        //    them is the failure mode this test pins down.
        let test_only = "fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn g() { \
                         Command::new(\"git\"); }\n}\n";
        assert!(
            production_hits("f.rs", test_only).is_empty(),
            "a spawn inside `#[cfg(test)] mod` is not a production spawn"
        );
        assert_eq!(scan("f.rs", test_only).len(), 1, "…but it is still SEEN");

        // 5. the combinator forms of the attribute, and the one that means the
        //    opposite (`outbound.rs` really uses `#[cfg(not(test))]`).
        assert!(production_hits(
            "f.rs",
            "#[cfg(any(test, feature = \"x\"))]\nmod t { fn g() { Command::new(\"git\"); } }\n"
        )
        .is_empty());
        assert!(production_hits(
            "f.rs",
            "#[cfg(all(test, windows))]\nmod t { fn g() { Command::new(\"git\"); } }\n"
        )
        .is_empty());
        assert_eq!(
            production_hits(
                "f.rs",
                "#[cfg(not(test))]\nfn g() { Command::new(\"git\"); }\n"
            )
            .len(),
            1,
            "`not(test)` selects a NON-test build — its body is production code"
        );

        // 6. a `#[cfg(test)]` on a `;`-terminated item must not swallow the
        //    rest of the file.
        assert_eq!(
            production_hits(
                "f.rs",
                "#[cfg(test)]\nuse std::process::Command;\nfn g() { Command::new(\"x\"); }\n"
            )
            .len(),
            1,
            "a `#[cfg(test)] use ...;` ends at its semicolon"
        );

        // 7. the non-`Command::new` mechanisms are covered.
        assert_eq!(
            production_hits("f.rs", "fn f() { pair.slave.spawn_command(cmd); }\n").len(),
            1,
            "portable-pty's spawn is a spawn"
        );
        assert_eq!(
            production_hits("f.rs", "fn f() { app.opener().open_url(u, None); }\n").len(),
            1,
            "handing a URL to the OS shell handler is a spawn"
        );

        // 8. the lexer keeps its sync across the awkward literals real Rust
        //    files contain — a quote in a char literal, an escaped quote, a
        //    lifetime that is not a literal at all.
        let awkward = "fn f<'a>(x: &'a str) { let q = '\"'; let e = '\\''; let b = b'-'; \
                       Command::new(x); }\n";
        assert_eq!(
            production_hits("f.rs", awkward).len(),
            1,
            "char literals containing quotes must not desync the string lexer"
        );

        // 9. the comparison rejects known-bad ledgers — including the empty
        //    parse, which is the one that would otherwise pass silently.
        let expected = ledger_counts();
        assert!(
            audit_against(&[], &expected).is_err(),
            "an empty file list MUST fail"
        );
        assert!(
            audit_against(
                &[(
                    "made_up/rogue.rs".to_string(),
                    "fn f() { Command::new(\"x\"); }\n".to_string()
                )],
                &expected
            )
            .is_err(),
            "an unledgered spawning file MUST fail"
        );
        assert!(
            audit_against(
                &[(
                    "procutil.rs".to_string(),
                    "fn f() { Command::new(\"a\"); Command::new(\"b\"); }\n".to_string()
                )],
                &expected
            )
            .is_err(),
            "a file with MORE spawns than its ledger row MUST fail"
        );
        // A ledger with a row for a file that no longer spawns anything fails too.
        let mut stale = BTreeMap::new();
        stale.insert("gone.rs", 1usize);
        assert!(
            audit_against(
                &[("procutil.rs".to_string(), "fn f() {}\n".to_string())],
                &stale
            )
            .is_err(),
            "a stale ledger row MUST fail"
        );
    }
}
