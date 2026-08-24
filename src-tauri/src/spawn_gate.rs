//! **The spawn gate** — the one lock every process spawn in this crate passes
//! through, so that no cImp-visible spawn can overlap the sandbox's inheritable
//! window.
//!
//! # The race
//!
//! `CreateProcessW(.., bInheritHandles = TRUE, ..)` does not inherit "the
//! handles you meant". It inherits **every handle in the calling process that
//! is marked inheritable at that instant** — a rule that is process-wide, not
//! per-spawn (KB 315939). So two threads spawning at the same time cross-
//! contaminate: thread A's pipe write-end, inheritable for the microseconds
//! around its own `CreateProcessW`, lands in thread B's child, and stays open
//! there for as long as B's child lives.
//!
//! That is not hypothetical here. It is what wedged the first live sandboxed
//! `run_command` on 2026-08-18: the sandbox's stdout pipe write-end leaked into
//! a concurrently-spawned cImp child, the sandboxed program exited, and the
//! drain thread's `ReadFile` never saw EOF because a stranger still held the
//! write end. The parent's `join()` never returned and the offload worker's
//! single slot was pinned for 22 minutes.
//!
//! # Why the standard library does not already solve this
//!
//! It solves it *for itself*. `std::process` holds a private process-wide
//! `CREATE_PROCESS_LOCK` (a `StaticMutex` in `sys::pal::windows::process`)
//! across the window in which it marks its stdio handles inheritable and calls
//! `CreateProcessW`, and `tokio::process` inherits that protection because it
//! spawns through `std`. But the lock is **private** — there is no public API
//! to take it — and cImp has two spawn paths that do not go through `std` at
//! all:
//!
//! * [`crate::sandbox::windows`]'s bespoke `CreateProcessW`, which exists
//!   precisely because neither `std` nor `tokio` can attach an AppContainer
//!   `SECURITY_CAPABILITIES` attribute list on stable Rust; and
//! * the PTY seam, where portable-pty builds its own `STARTUPINFOEX` and calls
//!   `CreateProcessW` itself.
//!
//! Two spawns outside a lock that everything else is inside is exactly one lock
//! too few. This module is the lock cImp *can* take, wrapped around every spawn
//! it owns — including the `std`/`tokio` ones, because taking `std`'s lock is
//! not the same as taking ours and the sandbox needs mutual exclusion against
//! **all** of them.
//!
//! # The invariant
//!
//! > **Every process spawn in this crate holds the gate — SHARED for ordinary
//! > spawns, EXCLUSIVE for any spawn whose handles are inheritable beyond a
//! > handle list.**
//!
//! Shared is the common case and costs nothing: ordinary spawns do not conflict
//! with each other, because each one's inheritable window is protected against
//! the others by `std`'s own lock, and none of them is the sandbox. Exclusive
//! has exactly one holder — [`crate::sandbox::windows::spawn_blocking_inner`],
//! which flips three handles to inheritable, calls `CreateProcessW`, and closes
//! them again inside the guard's scope. While it holds the write lock, nothing
//! else in cImp can be inside a spawn.
//!
//! [`tests::no_process_spawn_call_escapes_the_gate`] is what keeps this true:
//! a new `.spawn()` / `.output()` / `.status()` that does not route through this
//! module fails the suite with a message pointing here.
//!
//! # Guards are never held across an `.await`
//!
//! This is structural rather than a rule to remember. Every entry point below
//! takes the guard, does one synchronous thing, and drops it before returning —
//! there is no API that hands a caller a live shared guard, and the one that
//! hands out an exclusive guard ([`exclusive`]) is used by a single blocking
//! function. `tokio::process::Command::spawn` is itself synchronous, so
//! [`spawn_tokio`] needs no `async` at all.
//!
//! # What is NOT covered (the honest residual)
//!
//! Spawns made by third-party code deep inside libraries — the webview host
//! process, a Tauri plugin, anything that shells out without telling us — are
//! outside this gate, because gating them would mean gating code we do not
//! call. For that sliver, the defence remains the other half of the
//! 2026-08-18 fix: the sandbox's drains are bounded (grace →
//! `CancelSynchronousIo` → detach), so a leaked write end degrades one capture
//! instead of wedging the worker forever.

use std::sync::{RwLock, RwLockWriteGuard};

/// The gate itself. `()` is the whole payload — this lock protects a *global
/// process property* (which handles are currently inheritable), not any data
/// structure, so there is nothing to guard but the critical section.
///
/// Poisoning is meaningless for the same reason: a panic inside a spawn cannot
/// corrupt `()`, so every acquisition below recovers the guard rather than
/// propagating a poison error and turning one panicking spawn into a
/// permanently unspawnable process.
static GATE: RwLock<()> = RwLock::new(());

/// Run `f` while holding the gate SHARED.
///
/// The escape hatch for spawns that happen inside a third-party call, where
/// there is no `Command` for [`spawn_std`] / [`spawn_tokio`] to take: the PTY
/// seam (portable-pty's ConPTY spawn) and the OS opener.
///
/// # Contract
///
/// **The closure must contain ONLY the spawn call.** No `.await` (a shared
/// guard held across a yield point would block the sandbox for as long as the
/// task is parked), no reads of the child's output, no long blocking work. The
/// closure returns the spawn's result and the caller does everything else
/// outside.
///
/// # Not for `Command::output()` / `Command::status()`
///
/// Both spawn internally, so wrapping them would compile and would even be
/// correct — but neither is a *spawn call*: `std`'s runs the child to
/// completion inside the closure, which holds the shared guard for as long as
/// the child lives and (because `RwLock` lets a waiting writer hold new readers
/// off) stalls every other spawn in the app behind it; `tokio`'s spawns eagerly
/// and returns a future, which is correct only for as long as that undocumented
/// eagerness lasts. Every such site in this crate was therefore rewritten as
/// [`spawn_std`] / [`spawn_tokio`] plus an explicit `wait_with_output()`, which
/// is what those methods do internally anyway.
pub fn with_shared<T>(f: impl FnOnce() -> T) -> T {
    let _guard = GATE.read().unwrap_or_else(|e| e.into_inner());
    f()
}

/// Spawn a `std::process::Command` under the gate.
///
/// A pure routing wrapper: the command's program, arguments, environment and
/// stdio are whatever the caller configured, and nothing here touches them.
pub fn spawn_std(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    with_shared(|| cmd.spawn())
}

/// Spawn a `tokio::process::Command` under the gate.
///
/// `tokio::process::Command::spawn` is synchronous — it spawns the child and
/// registers it with the runtime, returning a `Child` that is awaited later —
/// so this needs no `async` and holds the guard across no yield point.
pub fn spawn_tokio(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    with_shared(|| cmd.spawn())
}

/// The RAII EXCLUSIVE window: while one of these is alive, no other spawn in
/// cImp can be in flight.
///
/// Returned by [`exclusive`]. Named rather than `impl Drop` so the type shows
/// up in signatures and so `#[must_use]` can say what dropping it immediately
/// would mean.
#[must_use = "dropping the window immediately re-opens the race it exists to close; \
              bind it to a named local whose scope is the inheritable window"]
pub struct ExclusiveSpawnWindow {
    /// Never read — held, then dropped. That drop IS the API: it is what
    /// closes the exclusive window.
    #[allow(dead_code)]
    guard: RwLockWriteGuard<'static, ()>,
}

/// Take the gate EXCLUSIVELY for the lifetime of the returned value.
///
/// **One caller only:** the sandbox engine, around the few syscalls in which
/// its pipe write-ends and NUL stdin handle are marked inheritable. Anything
/// else that needs to spawn should use [`spawn_std`] / [`spawn_tokio`] /
/// [`with_shared`].
///
/// # Deadlock discipline
///
/// The scope this guards must contain **nothing but** the handle flips, the
/// `CreateProcessW`, and the closing of those handles. No other lock may be
/// taken inside it, no allocation that could reach a spawning path, no
/// `.await`. `RwLock` is not reentrant, so a spawn attempted from inside the
/// exclusive scope would deadlock the process against itself.
pub fn exclusive() -> ExclusiveSpawnWindow {
    ExclusiveSpawnWindow {
        guard: GATE.write().unwrap_or_else(|e| e.into_inner()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rustsrc::{code_of, source_files, test_regions};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    /// Generous everywhere it can only make the test PASS more slowly, tight
    /// only where a wrong answer is the point. A CI box under load must not be
    /// able to flake these.
    const PATIENT: Duration = Duration::from_secs(10);
    /// How long "the blocked thread has definitely not run" is observed for.
    /// Being slower than this cannot produce a false failure — the flag can
    /// only be set by a thread that got past the gate, which is the bug.
    const BLOCKED_FOR: Duration = Duration::from_millis(200);

    // ── gate semantics ────────────────────────────────────────────────────

    /// EXCLUSIVE excludes SHARED: while the sandbox's window is open, no
    /// ordinary spawn can be in flight.
    #[test]
    fn an_exclusive_window_blocks_every_shared_spawn() {
        let entered = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<()>();
        let window = exclusive();

        let flag = Arc::clone(&entered);
        let t = std::thread::spawn(move || {
            with_shared(|| flag.store(true, Ordering::SeqCst));
            let _ = tx.send(());
        });

        std::thread::sleep(BLOCKED_FOR);
        assert!(
            !entered.load(Ordering::SeqCst),
            "a shared spawn got inside the gate while the sandbox held it EXCLUSIVELY — the \
             inheritable window is unprotected again"
        );

        drop(window);
        rx.recv_timeout(PATIENT)
            .expect("the shared spawn never ran after the exclusive window closed");
        assert!(entered.load(Ordering::SeqCst));
        t.join().expect("the shared waiter panicked");
    }

    /// …and the other direction: an in-flight SHARED spawn holds the exclusive
    /// window off. Without this the sandbox could flip its handles inheritable
    /// underneath a spawn that had already started.
    #[test]
    fn an_in_flight_shared_spawn_blocks_the_exclusive_window() {
        let (in_tx, in_rx) = mpsc::channel::<()>();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let took_exclusive = Arc::new(AtomicBool::new(false));

        // NB: this closure deliberately violates `with_shared`'s "only the
        // spawn call" contract by blocking inside the guard. That is the only
        // way to observe the exclusion from outside, and it is confined to
        // this test.
        let holder = std::thread::spawn(move || {
            with_shared(|| {
                let _ = in_tx.send(());
                let _ = go_rx.recv_timeout(PATIENT);
            });
        });
        in_rx
            .recv_timeout(PATIENT)
            .expect("the shared holder never entered the gate");

        let flag = Arc::clone(&took_exclusive);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let waiter = std::thread::spawn(move || {
            let _window = exclusive();
            flag.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        });

        std::thread::sleep(BLOCKED_FOR);
        assert!(
            !took_exclusive.load(Ordering::SeqCst),
            "the exclusive window opened while a shared spawn was still in flight"
        );

        let _ = go_tx.send(());
        holder.join().expect("the shared holder panicked");
        done_rx
            .recv_timeout(PATIENT)
            .expect("the exclusive window never opened after the shared spawn finished");
        waiter.join().expect("the exclusive waiter panicked");
    }

    /// SHARED does not exclude SHARED. Ordinary spawns are the overwhelming
    /// majority and must stay concurrent; a gate that serialized them would be
    /// a throughput regression dressed as a fix. Proven by requiring both
    /// threads to be inside the gate *at the same time* — if the lock were
    /// exclusive, the second `in` message would never arrive.
    #[test]
    fn two_shared_spawns_can_be_inside_the_gate_at_once() {
        let (in_tx, in_rx) = mpsc::channel::<u8>();
        let (go_a_tx, go_a_rx) = mpsc::channel::<()>();
        let (go_b_tx, go_b_rx) = mpsc::channel::<()>();

        let tx_a = in_tx.clone();
        let a = std::thread::spawn(move || {
            with_shared(|| {
                let _ = tx_a.send(b'a');
                let _ = go_a_rx.recv_timeout(PATIENT);
            });
        });
        let b = std::thread::spawn(move || {
            with_shared(|| {
                let _ = in_tx.send(b'b');
                let _ = go_b_rx.recv_timeout(PATIENT);
            });
        });

        let mut seen = BTreeSet::new();
        for _ in 0..2 {
            seen.insert(
                in_rx
                    .recv_timeout(PATIENT)
                    .expect("only one shared holder ever got inside — the gate is serializing \
                             ordinary spawns"),
            );
        }
        assert_eq!(seen.len(), 2, "both shared holders must be inside at once");

        let _ = go_a_tx.send(());
        let _ = go_b_tx.send(());
        a.join().expect("shared holder a panicked");
        b.join().expect("shared holder b panicked");
    }

    // ── the source scan ───────────────────────────────────────────────────
    //
    // Same discipline as `spawn_ledger`'s exhaustiveness tripwire and
    // `harness::layering`'s two scanners, and the same shared primitives
    // (`crate::rustsrc`): comments/strings/chars are blanked with a
    // self-check first, `#[cfg(test)]` items are located by brace-matched
    // spans rather than a single textual cut, and `\r` is stripped BEFORE any
    // offset is taken so a CRLF CI checkout scans the same bytes as an LF
    // working copy.

    /// The spawn *constructors*, `concat!`-assembled so this file does not
    /// match its own needles. Deliberately the same set as
    /// `spawn_ledger`'s — and
    /// [`the_scanned_file_set_is_exactly_the_spawn_ledgers`] asserts the two
    /// stay in step, so a new spawn MECHANISM cannot be taught to one scanner
    /// and not the other.
    const CTOR_NEEDLES: &[&str] = &[
        concat!("Command::", "new"),
        concat!("spawn_", "command"),
        concat!("open_", "url"),
        concat!("open_", "path"),
        concat!("CreateProcess", "W"),
    ];

    /// The spawn *calls*. Each one runs a child process; each one must be
    /// under the gate.
    const CALL_NEEDLES: &[&str] = &[
        concat!(".spawn", "()"),
        concat!(".output", "()"),
        concat!(".status", "()"),
    ];

    /// Files whose spawn call sites this scan does not police, with the reason.
    /// Self-checked by [`no_process_spawn_call_escapes_the_gate`]: an entry for
    /// a file that no longer spawns anything fails the test.
    const FILE_EXEMPT: &[(&str, &str)] = &[(
        "sandbox/windows.rs",
        "The exclusive holder. Its spawn is a bespoke `CreateProcessW`, not a `Command`, so no \
         call needle matches it in the first place — and it takes `spawn_gate::exclusive()` \
         rather than a shared guard, which the file-level check below verifies.",
    )];

    /// `.spawn()` / `.output()` / `.status()` occurrences in a spawning file
    /// that are **not** process spawns, as `(file, receiver + method, reason)`.
    ///
    /// `.status()` is the collision: `reqwest::Response::status()` and cImp's
    /// own service-state accessors share the spelling with
    /// `Command::status()`. Naming the receiver keeps each exemption to one
    /// expression instead of blanket-exempting a file, and
    /// [`no_process_spawn_call_escapes_the_gate`] fails if an entry stops
    /// matching an ungated hit — so an exemption cannot outlive its reason.
    /// V42 Phase A1-3 retired this list's two `ipc/commands.rs` rows rather
    /// than moving them. Both were an offload accessor's `.status()` colliding
    /// with `Command::status()` in the one file that also opens a folder; those
    /// bodies are `service::offload` use cases now, and a use case is named for
    /// what it answers (`primary_state`, `aggregate_state`), so the collision no
    /// longer exists to exempt. An exemption that can be DELETED is a better
    /// outcome than one that is moved.
    const CALL_EXEMPT: &[(&str, &str, &str)] = &[
        (
            "offload/mcp_host.rs",
            "resp.status()",
            "`reqwest::Response::status()` — an HTTP status code from the streamable-HTTP MCP \
             transport, not a child process.",
        ),
        (
            "offload/supervisor.rs",
            "resp.status()",
            "`reqwest::Response::status()` on the llama-server health probe.",
        ),
    ];

    /// One needle occurrence in production (non-`#[cfg(test)]`) code.
    struct Hit {
        line: usize,
        needle: &'static str,
        /// Byte offset of the needle in the blanked code.
        at: usize,
    }

    /// Production-code occurrences of `needles` in `src`, with the blanked code
    /// returned alongside so callers can look at the surrounding statement.
    fn production_hits(rel: &str, src: &str, needles: &[&'static str]) -> (String, Vec<Hit>) {
        let code = code_of(rel, src);
        let regions = test_regions(&code);
        let mut hits = Vec::new();
        for needle in needles {
            let mut from = 0usize;
            while let Some(off) = code[from..].find(needle) {
                let at = from + off;
                from = at + needle.len();
                if regions.iter().any(|(s, e)| at >= *s && at < *e) {
                    continue;
                }
                hits.push(Hit {
                    line: code[..at].matches('\n').count() + 1,
                    needle,
                    at,
                });
            }
        }
        hits.sort_by_key(|h| h.at);
        (code, hits)
    }

    /// The receiver expression immediately before a `.method()` hit — the
    /// identifier path the call is made on, or `""` when the call continues a
    /// multi-line method chain (`cmd\n    .spawn()`).
    fn receiver_of(code: &str, at: usize) -> &str {
        let b = code.as_bytes();
        let mut start = at;
        while start > 0 {
            let c = b[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b':' {
                start -= 1;
            } else {
                break;
            }
        }
        &code[start..at]
    }

    /// The text from the start of the statement containing `at` up to `at`.
    /// "Statement" is approximated by the nearest preceding `;`, `{` or `}`,
    /// which is enough to tell `spawn_gate::with_shared(|| cmd.output())` from
    /// a bare `cmd.output()` two lines below one.
    fn statement_before(code: &str, at: usize) -> &str {
        let b = code.as_bytes();
        let mut start = at;
        while start > 0 {
            match b[start - 1] {
                b';' | b'{' | b'}' => break,
                _ => start -= 1,
            }
        }
        &code[start..at]
    }

    /// Is this call site lexically under the gate?
    fn is_gated(window: &str) -> bool {
        window.contains(concat!("spawn_", "gate::")) || window.contains(concat!("with_", "shared("))
    }

    /// Files that construct a process in production code — the only files a
    /// spawn call can live in.
    fn spawning_files(files: &[(String, String)]) -> BTreeMap<String, String> {
        files
            .iter()
            .filter(|(rel, _)| rel != "spawn_gate.rs")
            .filter(|(rel, src)| !production_hits(rel, src, CTOR_NEEDLES).1.is_empty())
            .map(|(rel, src)| (rel.clone(), src.clone()))
            .collect()
    }

    // ── the tripwires ─────────────────────────────────────────────────────

    /// **The scan reads the same files `spawn_ledger` does.**
    ///
    /// This scan restricts itself to files that construct a process, which is
    /// sound only if that constructor set is complete — and the thing that
    /// makes it complete is `spawn_ledger::tests::the_spawn_ledger_is_exhaustive`,
    /// which walks the whole tree for the identical needles. Anchoring the two
    /// together means a new spawn MECHANISM (the way `CreateProcessW` once was)
    /// cannot be taught to the ledger and forgotten here, or vice versa.
    #[test]
    fn the_scanned_file_set_is_exactly_the_spawn_ledgers() {
        let files = source_files();
        let scanned: BTreeSet<String> = spawning_files(files).into_keys().collect();
        let ledgered: BTreeSet<String> = crate::spawn_ledger::ledger()
            .iter()
            .map(|s| s.file.to_string())
            .collect();
        assert_eq!(
            scanned, ledgered,
            "the spawn gate's scan and `spawn_ledger::ledger` disagree about which files spawn a \
             process. Whichever is behind, both have to move: the ledger classifies the seam, \
             this scan proves it goes through the gate."
        );
    }

    /// **Files that build a `Command` and hand it to a named delegate to
    /// spawn**, as `(spawning file, the delegate's file, the call spelling)`.
    ///
    /// V42 R27 extracted the confined-spawn walk out of the two agent seams
    /// that ran it line for line, so the `Command` is now BUILT in
    /// `audit/runner.rs` / `checks/mod.rs` and SPAWNED one hop away, in
    /// `sandbox/confine.rs`. The routing is still real; only the textual proof
    /// moved — and the answer to that is to prove the hop, never to exempt it.
    ///
    /// [`every_spawning_file_routes_through_the_spawn_gate`] accepts a caller
    /// listed here only after checking that the DELEGATE itself names the gate
    /// in production code. So the chain is verified end to end: a delegate that
    /// stops being gated fails its callers too, and a row whose caller no longer
    /// names the delegate is stale and fails on its own. One verified
    /// indirection, not a hole.
    ///
    /// Keep this list short on purpose. Every entry is a file whose spawn a
    /// reader cannot see by reading that file, which is a real cost — it is
    /// worth paying only where the alternative is a security boundary
    /// maintained in two copies.
    const GATE_DELEGATES: &[(&str, &str, &str)] = &[
        (
            "audit/runner.rs",
            "sandbox/confine.rs",
            concat!("confine::", "run_confined"),
        ),
        (
            "checks/mod.rs",
            "sandbox/confine.rs",
            concat!("confine::", "run_confined"),
        ),
    ];

    /// Does `rel` name `needle` outside its `#[cfg(test)]` regions?
    fn names_in_production(rel: &str, src: &str, needle: &'static str) -> bool {
        !production_hits(rel, src, &[needle]).1.is_empty()
    }

    /// **Every spawning file names the gate** (design invariant, module doc).
    ///
    /// The call-site scan below cannot see a spawn that has no `Command` —
    /// portable-pty's ConPTY spawn, the OS opener, the sandbox's bespoke
    /// `CreateProcessW`. This one can: whatever the mechanism, the file that
    /// uses it has to mention `spawn_gate::` — or hand its `Command` to a
    /// [`GATE_DELEGATES`] hop that does, which this test then verifies rather
    /// than assumes.
    #[test]
    fn every_spawning_file_routes_through_the_spawn_gate() {
        let files = source_files();
        let spawning = spawning_files(files);
        let by_path: BTreeMap<&str, &String> =
            files.iter().map(|(r, s)| (r.as_str(), s)).collect();
        let mut ungated = Vec::new();
        let mut used_delegates: BTreeSet<&str> = BTreeSet::new();
        for (rel, src) in &spawning {
            if names_in_production(rel, src, concat!("spawn_", "gate::")) {
                continue;
            }
            // V42 R27 — a one-hop route counts, but only once the hop is
            // checked. See [`GATE_DELEGATES`].
            let hop = GATE_DELEGATES
                .iter()
                .find(|(caller, _, call)| *caller == rel.as_str() && names_in_production(rel, src, call));
            match hop {
                Some((caller, delegate, call)) => {
                    let dsrc = by_path.get(delegate).unwrap_or_else(|| {
                        panic!("{caller} routes `{call}` through `{delegate}`, which is not in the tree")
                    });
                    assert!(
                        names_in_production(delegate, dsrc, concat!("spawn_", "gate::")),
                        "{caller} builds a `Command` and hands it to `{delegate}` to spawn, but \
                         `{delegate}` does not name the gate — the delegation proof is broken and \
                         BOTH files are ungated now"
                    );
                    used_delegates.insert(*caller);
                }
                None => ungated.push(rel.clone()),
            }
        }
        assert!(
            ungated.is_empty(),
            "these files spawn a process without ever naming `spawn_gate::` — route the spawn \
             through `spawn_gate::spawn_std` / `spawn_tokio`, or wrap the third-party spawn call \
             in `spawn_gate::with_shared(|| ..)`: {ungated:#?}"
        );
        // Self-check, same rule as the exemption lists: a delegation row whose
        // caller now names the gate itself (or no longer spawns at all) has
        // outlived its reason and must be DELETED, not left to rot.
        let stale: Vec<&str> = GATE_DELEGATES
            .iter()
            .map(|(caller, _, _)| *caller)
            .filter(|c| !used_delegates.contains(c))
            .collect();
        assert!(
            stale.is_empty(),
            "these GATE_DELEGATES rows no longer describe a delegating spawn site — delete \
             them: {stale:#?}"
        );
        // …and the sandbox is the ONE exclusive holder, by name.
        let sandbox = spawning
            .get("sandbox/windows.rs")
            .expect("the sandbox engine lost its spawn");
        assert!(
            code_of("sandbox/windows.rs", sandbox).contains(concat!("spawn_gate::", "exclusive()")),
            "sandbox/windows.rs no longer takes the EXCLUSIVE gate around its inheritable \
             window — a shared guard is not enough there, because the whole point is that its \
             pipe write-ends are inheritable while `CreateProcessW` runs"
        );
        let exclusive_users: Vec<&String> = spawning
            .iter()
            .filter(|(rel, src)| {
                rel.as_str() != "sandbox/windows.rs"
                    && code_of(rel, src).contains(concat!("spawn_gate::", "exclusive()"))
            })
            .map(|(rel, _)| rel)
            .collect();
        assert!(
            exclusive_users.is_empty(),
            "`spawn_gate::exclusive()` has exactly one legitimate caller (the sandbox engine); \
             an ordinary spawn taking the write lock serializes every spawn in the app: \
             {exclusive_users:#?}"
        );
    }

    /// **No process-spawn call escapes the gate.**
    ///
    /// The call-site half. Every production `.spawn()` / `.output()` /
    /// `.status()` in a file that constructs a process must sit inside a
    /// statement that names the gate — or be listed in [`CALL_EXEMPT`] as not a
    /// spawn at all, with a reason.
    #[test]
    fn no_process_spawn_call_escapes_the_gate() {
        let files = source_files();
        let spawning = spawning_files(files);
        let file_exempt: BTreeMap<&str, &str> = FILE_EXEMPT.iter().copied().collect();

        let mut violations: Vec<String> = Vec::new();
        let mut used_call_exempt: BTreeSet<(&str, &str)> = BTreeSet::new();
        let mut gated_sites = 0usize;

        for (rel, src) in &spawning {
            if file_exempt.contains_key(rel.as_str()) {
                continue;
            }
            let (code, hits) = production_hits(rel, src, CALL_NEEDLES);
            for hit in hits {
                if is_gated(statement_before(&code, hit.at)) {
                    gated_sites += 1;
                    continue;
                }
                let site = format!("{}{}", receiver_of(&code, hit.at), hit.needle);
                if let Some((f, s, _)) = CALL_EXEMPT
                    .iter()
                    .find(|(f, s, _)| *f == rel.as_str() && *s == site)
                {
                    used_call_exempt.insert((f, s));
                    continue;
                }
                violations.push(format!(
                    "{rel}:{} — `{site}` spawns a process outside the spawn gate. Route it \
                     through `spawn_gate::spawn_std` / `spawn_tokio`, or wrap it in \
                     `spawn_gate::with_shared(|| ..)`. If it is not a process spawn at all \
                     (an HTTP `status()`, a service accessor), add it to CALL_EXEMPT with the \
                     reason.",
                    hit.line
                ));
            }
        }

        assert!(
            violations.is_empty(),
            "ungated process spawns — every one of these can inherit the sandbox's pipe \
             write-ends and wedge a sandboxed run (see `spawn_gate`'s module doc):\n  {}",
            violations.join("\n  ")
        );
        // Non-vacuity. `gated_sites` is deliberately NOT asserted to be
        // non-zero: every routed site now calls `spawn_gate::spawn_std` /
        // `spawn_tokio` (or wraps a third-party call), so the *text*
        // `.spawn()` has disappeared from those files entirely — which is the
        // outcome this scan wants, and would make a "we saw a gated site"
        // assertion permanently red. What proves the scan read real code is
        // `source_files`' file-count floor, the ledger-equality test above, and
        // the exemptions below still matching real hits.
        let _ = gated_sites;
        assert!(
            spawning.len() >= 10,
            "the scan found only {} spawning files — it read nothing useful, and a scan that \
             passes on an empty parse is the failure mode this design exists against",
            spawning.len()
        );

        // Self-check, both exemption lists: an exemption that no longer matches
        // anything is padding that silently widens the next person's blast
        // radius.
        let stale_calls: Vec<String> = CALL_EXEMPT
            .iter()
            .filter(|(f, s, _)| !used_call_exempt.contains(&(*f, *s)))
            .map(|(f, s, _)| format!("{f}: `{s}`"))
            .collect();
        assert!(
            stale_calls.is_empty(),
            "these CALL_EXEMPT entries no longer match an ungated call site — delete them: \
             {stale_calls:#?}"
        );
        let stale_files: Vec<&str> = FILE_EXEMPT
            .iter()
            .map(|(f, _)| *f)
            .filter(|f| !spawning.contains_key(*f))
            .collect();
        assert!(
            stale_files.is_empty(),
            "these FILE_EXEMPT entries name files that no longer spawn anything — delete them: \
             {stale_files:#?}"
        );
    }

    /// The scan's own controls, on input whose answer is written down rather
    /// than inferred from the tree. A tripwire nobody has watched fail is an
    /// assumption.
    #[test]
    fn the_scan_finds_what_it_claims_to_find() {
        // The fixtures spell the needles inside string literals, which the
        // blanking pass erases before any search — so this file stays out of
        // its own scan.
        let ungated = "fn f() { let mut c = Command::new(\"x\"); c.spawn(); }\n";
        let (code, hits) = production_hits("f.rs", ungated, CALL_NEEDLES);
        assert_eq!(hits.len(), 1, "a bare production spawn call must be found");
        assert_eq!(receiver_of(&code, hits[0].at), "c");
        assert!(!is_gated(statement_before(&code, hits[0].at)));

        // A gated call, in each of the two shapes production uses.
        let wrapped = "fn f() { let o = crate::spawn_gate::with_shared(|| c.output()); }\n";
        let (code, hits) = production_hits("f.rs", wrapped, CALL_NEEDLES);
        assert_eq!(hits.len(), 1);
        assert!(
            is_gated(statement_before(&code, hits[0].at)),
            "a `with_shared(|| ..)` wrap must read as gated"
        );
        let chained = "fn f() {\n    let o = crate::spawn_gate::with_shared(|| cmd\n        \
                       .output());\n}\n";
        let (code, hits) = production_hits("f.rs", chained, CALL_NEEDLES);
        assert_eq!(hits.len(), 1);
        assert!(
            is_gated(statement_before(&code, hits[0].at)),
            "a multi-line chain inside the wrap must still read as gated"
        );

        // …and the same call two statements later does NOT inherit that.
        let leaky = "fn f() { let a = crate::spawn_gate::spawn_std(&mut c); let b = d.spawn(); }\n";
        let (code, hits) = production_hits("f.rs", leaky, CALL_NEEDLES);
        assert_eq!(hits.len(), 1, "`spawn_std(..)` itself is not a call needle");
        assert_eq!(receiver_of(&code, hits[0].at), "d");
        assert!(
            !is_gated(statement_before(&code, hits[0].at)),
            "adjacency must not leak across a `;` — otherwise one gated spawn would launder \
             every ungated one beside it"
        );

        // Test code is not production code…
        let in_test = "fn p() {}\n#[cfg(test)]\nmod t { fn g() { c.spawn(); } }\n";
        assert!(
            production_hits("f.rs", in_test, CALL_NEEDLES).1.is_empty(),
            "a spawn inside `#[cfg(test)]` is not a production spawn"
        );
        // …but `not(test)` is.
        let not_test = "#[cfg(not(test))]\nfn g() { c.spawn(); }\n";
        assert_eq!(production_hits("f.rs", not_test, CALL_NEEDLES).1.len(), 1);

        // Comments and literals are not code.
        assert!(
            production_hits("f.rs", "// call c.spawn() here\nfn f() {}\n", CALL_NEEDLES)
                .1
                .is_empty()
        );
        assert!(
            production_hits("f.rs", "fn f() { let s = \"c.spawn()\"; }\n", CALL_NEEDLES)
                .1
                .is_empty()
        );

        // The receiver is what tells an HTTP `status()` from a `Command` one.
        let http = "fn f() { if !resp.status().is_success() {} }\n";
        let (code, hits) = production_hits("f.rs", http, CALL_NEEDLES);
        assert_eq!(hits.len(), 1);
        assert_eq!(receiver_of(&code, hits[0].at), "resp");

        // The constructor scan is what decides which files are looked at.
        let files = vec![
            ("a.rs".to_string(), "fn f() { Command::new(\"x\"); }\n".to_string()),
            ("b.rs".to_string(), "fn f() { let x = 1; }\n".to_string()),
        ];
        let spawning = spawning_files(&files);
        assert!(spawning.contains_key("a.rs") && !spawning.contains_key("b.rs"));
    }
}
