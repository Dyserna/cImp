//! Client-side instance discovery — how a cImp-spawned child finds the app it
//! belongs to, and how the app publishes itself to be found.
//!
//! The loopback listener (`super::loopback`) advertises `{port, token, pid,
//! root}` in a discovery file next to the exe (the portable root — never
//! `~/.claude`). This module is the OTHER half: the reader. It lives on the
//! CHILD side of that seam — the `--offload-mcp` proxy, the audit MCP server,
//! the harness plugins, the sandbox grant table — where "which instance?" is a
//! real question, because one install can have several cImp windows open on
//! different projects and a child that picks the wrong one runs its audits and
//! graph queries against the WRONG project.
//!
//! Two halves, and the split matters:
//!
//! - **Selection** (`select_discovery` / `select_verified` / `responds`) is
//!   pure and app-state-free: given the entries on disk and a cwd hint, which
//!   one is this child's? Deepest root-containing entry that still answers a
//!   `GET /health` probe, with the legacy single file as the last fallback.
//! - **Reporting** (`report_skipped_to_app` and friends) tells the app it
//!   skipped candidates, so a misresolution is visible in Events rather than
//!   silent. It is fire-and-forget, bounded, and deliberately says nothing
//!   back to the caller.
//!
//! Extracted verbatim from `loopback.rs` (V42 R2, #114) — the code, its
//! visibility, and its behaviour are unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tracing::{debug, info, warn};

/// Discovery-file name under the portable root (next to `settings.json`).
/// Legacy single-instance location, still written for anything that only
/// knows this path; the per-instance directory below is authoritative.
///
/// `pub(crate)` so the sandbox grant table (`sandbox::tabs`) names the file the
/// proxy child actually reads rather than a second spelling of it — a rename
/// here would otherwise leave a sandboxed tab's child unable to find the app,
/// with nothing but a denial row to say why.
pub(crate) const DISCOVERY_FILE: &str = ".cimp-offload.json";

/// Per-instance discovery DIRECTORY under the portable root: one
/// `<pid>.json` per running instance, each carrying that instance's launch
/// `root`. The legacy single file is last-writer-wins, so with two cImp
/// instances open a child spawned by project A's agent could connect to
/// project B's app — and audits/graph queries would run against the WRONG
/// project. Readers resolve root-aware via [`read_discovery_for`].
///
/// `pub(crate)` for the same reason as [`DISCOVERY_FILE`].
pub(crate) const DISCOVERY_DIR: &str = ".cimp-discovery";

/// Total wall-clock budget for ONE candidate's liveness probe ([`responds`]) —
/// connect, write and read together, not per syscall.
///
/// Locked decision 30 (#48 F-11) put a network round trip on the resolution path
/// every stdio child and every hook shim takes, so the bound is part of the
/// decision rather than a tuning knob: **a probe with no timeout would turn a
/// dead port into a hang, which is worse than the finding.** A refused loopback
/// connect returns in microseconds, so this budget is only ever spent against
/// something that is listening and not answering — which, per the accepted case
/// against decision 30, means an attacker who has already paid for a listener.
///
/// `pub(crate)` since the 2026-08-13 amendment: it is part of the wall-clock a
/// `PreToolUse` hook can spend, and `checkpoint_beacon` now asserts the whole
/// worst case against the harness's hook ceiling.
pub(crate) const DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// How many candidates one resolution will probe, across all three preference
/// steps ([`select_verified`]).
///
/// The other half of the bound: without it, N planted entries cost N probes and
/// the attacker sets N. Worst case per COLD resolution is therefore
/// `MAX_DISCOVERY_PROBES * DISCOVERY_PROBE_TIMEOUT` = **1.2 s**, once per process
/// per hint (the winner is memoized — see [`read_discovery_for`]).
///
/// Consequence, stated rather than hidden: flooding `.cimp-discovery/` with more
/// than this many *deeper* well-formed entries pushes the real instance past the
/// budget and the child goes headless. That is not a regression — today ONE such
/// entry already wins and the child goes headless — and headless is governed by
/// M-8's `--tab` rule, which does not consult the reason. Enforced by
/// `tests::a_resolution_never_probes_more_than_its_budget`.
///
/// `pub(crate)` for the same reason as [`DISCOVERY_PROBE_TIMEOUT`].
pub(crate) const MAX_DISCOVERY_PROBES: usize = 6;

/// The discovery file the child reads to find + authenticate to the app.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The launch project root this instance serves (canonicalized at
    /// write). `#[serde(default)]` — absent in legacy files.
    #[serde(default)]
    pub root: String,
}

/// The portable root (exe dir), falling back to the cwd if `current_exe()`
/// is unavailable (mirrors `settings::global_path`).
fn portable_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `<exe-dir>/.cimp-offload.json` — the legacy portable-root discovery path.
pub fn discovery_path() -> PathBuf {
    portable_root().join(DISCOVERY_FILE)
}

/// `<exe-dir>/.cimp-discovery/` — the per-instance discovery directory.
fn discovery_dir() -> PathBuf {
    portable_root().join(DISCOVERY_DIR)
}

/// This process's per-instance discovery file.
pub(super) fn own_discovery_path() -> PathBuf {
    discovery_dir().join(format!("{}.json", std::process::id()))
}

/// Read the legacy single discovery file, if present and parseable.
pub fn read_discovery() -> Option<Discovery> {
    let text = std::fs::read_to_string(discovery_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every parseable per-instance discovery entry (stale ones included — they
/// are swept at instance start; see [`sweep_stale_discoveries`]).
pub(super) fn read_all_discoveries() -> Vec<Discovery> {
    let Ok(entries) = std::fs::read_dir(discovery_dir()) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect()
}

/// This instance's own discovery entry — the per-instance file first, then
/// the legacy file when it still belongs to this pid. Used by in-app writers
/// that bake port+token into generated artifacts (the OpenCode plugin) and
/// must never pick up a sibling instance's endpoint.
pub fn read_own_discovery() -> Option<Discovery> {
    if let Ok(text) = std::fs::read_to_string(own_discovery_path()) {
        if let Ok(d) = serde_json::from_str::<Discovery>(&text) {
            return Some(d);
        }
    }
    read_discovery().filter(|d| d.pid == std::process::id())
}

/// Canonicalized-or-raw form of a path for ancestry comparison. Both the
/// writer (instance root) and readers (child cwd) go through this, so the
/// `\\?\` extended prefix `std::fs::canonicalize` adds on Windows appears on
/// both sides and cancels out in the component-wise comparison.
pub(super) fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Component-wise "is `root` an ancestor of (or equal to) `hint`" — case
/// insensitive on Windows, where on-disk casing and agent-reported cwds
/// routinely disagree.
///
/// **An unresolved `..` on either side is refused, never matched** (V33). This
/// walk compares components literally: it cannot resolve a
/// [`Component::ParentDir`](std::path::Component::ParentDir), and its partner
/// [`canon`] only resolves one when the path EXISTS — otherwise `canonicalize`
/// fails and the raw path is kept verbatim. So `<root>/../../evil` arrives here
/// still carrying its `..`, and a plain zip-compare answers "descendant of
/// `<root>`" because the leading components genuinely do match. The refusal
/// lives here rather than at either caller so that both of them —
/// [`audit_admit`] step 3 and [`admitted_hook_root`] — get the same answer by
/// construction; a per-route copy is exactly the check that drifts.
///
/// This is written from a measurement, because the tempting "Windows already
/// rejects it" rebuttal is wrong in both directions. `canon` adds a `\\?\`
/// verbatim prefix when it succeeds and not when it fails, so
/// `P:\root\..\..\evil` is rejected only on an accidental *prefix* mismatch —
/// while `\\?\P:\root\..\..\evil`, which a caller may simply spell that way,
/// matches on the prefix and walks straight through. On Linux there is no
/// prefix at all and the plain spelling walks through too. An accident is not a
/// check, which is why this is one.
pub(super) fn is_ancestor_or_equal(root: &Path, hint: &Path) -> bool {
    let rc: Vec<_> = root.components().collect();
    let hc: Vec<_> = hint.components().collect();
    if rc.is_empty() || rc.len() > hc.len() {
        return false;
    }
    if rc
        .iter()
        .chain(hc.iter())
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return false;
    }
    rc.iter().zip(hc.iter()).all(|(a, b)| {
        let (a, b) = (
            a.as_os_str().to_string_lossy(),
            b.as_os_str().to_string_lossy(),
        );
        if cfg!(windows) {
            a.eq_ignore_ascii_case(&b)
        } else {
            a == b
        }
    })
}

// ── #104: an externally supplied cwd is never a project root by itself ──────

/// The directory a configured `tab` launches in, or `None` when the id names no
/// AI tab of this instance.
///
/// The **one legitimate root source** (#104 item 2): it comes from the user's
/// own tab configuration through [`crate::tabs::ai_tab_dir`] — the same call the
/// spawner makes — so it is a directory cImp itself chose, not one a caller
/// asserted. `claude_hook_cwd` and [`external_project_root`] share it so the tab
/// a hook claims resolves to the same place on both paths.
pub(crate) fn hook_tab_root(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    tab: Option<&str>,
) -> Option<PathBuf> {
    let tab = tab.map(str::trim).filter(|t| !t.is_empty())?;
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok())?;
    crate::tabs::ai_tab_dir(settings, tab, &launch)
}

/// The project root an externally supplied `cwd` names — `None` to refuse.
///
/// **#104.** Every `cwd` reaching this file came from a process cImp does not
/// control: a Claude hook payload, the generated OpenCode plugin, an MCP call
/// body. A sub-agent's shell keeps its cwd across calls, so one `cd` into
/// `src-tauri/src/harness` made every later hook report that as its working
/// directory — and the routes below took it as a project root, attributed their
/// Activity rows to it and had the graph/workbench layer MINT per-project state
/// there (`<db_subdir>/graph.db`, `<db_subdir>/shadow.git`). Ten such
/// directories under one repo is the issue.
///
/// Three steps, in this order:
///
/// 1. **The tab's own configured directory wins** when the cwd is at or under
///    it. A tab rooted at a sub-project inside a larger repo is a real project
///    whose root only the tab configuration knows, and step 2's walk would
///    otherwise attribute it to the enclosing repo.
/// 2. **Otherwise walk UP** to the nearest marker
///    ([`crate::fsutil::find_project_root`] — `.git`, dir or file, beats an
///    existing `<db_subdir>`; nearest wins). A `<db_subdir>` found strictly
///    below the answer is reported through [`report_stray_state`] and left
///    alone.
/// 3. **No marker anywhere** ⇒ the tab's directory if a tab is known, else
///    `None`. `None` means REFUSED as a root: the caller records the row with an
///    empty `root` (the honest "attributed to no project" value) and creates
///    nothing. A genuinely new, un-VCS'd folder still gets its state, because a
///    tab is configured for it and step 3 names it.
///
/// The answer is returned in the PLAIN spelling (no extended-length prefix), so
/// `rel_path`'s root-prefix strip and `activity::root_key` see the same form the
/// tab configuration and the indexer use.
pub(super) fn external_project_root(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    tab: Option<&str>,
    cwd: Option<&str>,
) -> Option<PathBuf> {
    resolve_external_root(
        hook_tab_root(app, settings, tab),
        cwd,
        &settings.graph.effective_db_subdir(),
    )
}

/// [`external_project_root`]'s decision, with the two app-derived inputs already
/// resolved — the tab's configured directory and the configured state
/// subdirectory name.
///
/// Split out so the rule itself is unit-testable: the wrapper needs an
/// `AppHandle`, which no test in this crate can construct, and #104 is a
/// question about *which directory wins*, not about tauri state. Every branch
/// below is exercised by `resolve_external_root_*` in this file's tests.
pub(super) fn resolve_external_root(
    tab_root: Option<PathBuf>,
    cwd: Option<&str>,
    db_subdir: &str,
) -> Option<PathBuf> {
    let Some(raw) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return tab_root;
    };
    let given = canon(Path::new(raw));
    // 1. The tab's own root, when the cwd is inside it.
    if let Some(tr) = &tab_root {
        if is_ancestor_or_equal(&canon(tr), &given) {
            return Some(tr.clone());
        }
    }
    // 2. The nearest marker up the chain.
    if let Some(found) = crate::fsutil::find_project_root(&given, db_subdir) {
        if let Some(stray) = &found.stray_state {
            report_stray_state(&found.root, stray);
        }
        return Some(PathBuf::from(crate::fsutil::plain_path(
            &found.root.to_string_lossy(),
        )));
    }
    // 3. No marker: the tab's root, or refuse.
    tab_root
}

/// Stray `<db_subdir>` directories already reported this process.
///
/// One row per path per run, not one per hook call: resolution runs on every
/// prompt, read and tool result, and an unbounded repeat would evict the graph
/// lane it is recorded in — the very failure #51's per-lane retention exists to
/// prevent. Bounded, because the set is derived from caller-supplied cwds.
static STRAY_STATE_SEEN: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();

/// The most distinct stray paths one run will remember (and therefore report).
const MAX_STRAY_STATE_SEEN: usize = 64;

/// Report — never remove — a `<db_subdir>` directory found BELOW a resolved
/// project root (#104 item 7).
///
/// **Reported, not swept.** The directory holds a `graph.db` and a `shadow.git`;
/// they are the user's data, and an app that silently deletes state it decides
/// is misplaced is a worse failure than the one being fixed. So this says where
/// it is and stops there — the operator removes it (cImp holds `graph.db` open
/// while it runs, so the removal wants the app closed anyway).
///
/// The row lands in the **graph** lane with `source = "project_root"`, which is
/// where the read advisor's and auto-check's own structural rows already live
/// and where the `<db_subdir>` this is about belongs. It is deliberately not a
/// new [`crate::activity::ActivityKind`]: a kind is a retention lane plus a UI
/// filter (#51), and one warning class does not earn either. `ok = false`, so
/// the Events tab shows it as something to act on.
fn report_stray_state(root: &Path, stray: &Path) {
    {
        let seen = STRAY_STATE_SEEN.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
        let Ok(mut seen) = seen.lock() else {
            return;
        };
        if !seen.insert(stray.to_path_buf()) {
            return;
        }
        if seen.len() > MAX_STRAY_STATE_SEEN {
            seen.clear();
        }
    }
    let stray_s = crate::fsutil::plain_path(&stray.to_string_lossy());
    let root_s = crate::fsutil::plain_path(&root.to_string_lossy());
    warn!(
        target: "offload",
        stray = %stray_s,
        root = %root_s,
        "project state directory found BELOW this project's root — cImp minted it from a \
         sub-agent's working directory (#104) and no longer uses it. Nothing here reads or \
         writes it; remove it by hand with cImp closed if you do not want it."
    );
    crate::activity::record_bg(crate::activity::ActivityRecord {
        request: format!("resolved root {root_s}"),
        response: String::new(),
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            crate::activity::root_key(root),
            "project_root".to_string(),
            "stray_state".to_string(),
            stray_s,
            0,
            0,
            false,
            crate::activity::Attribution::Headless,
            None,
            None,
            None,
        ),
    });
}

/// Whether the endpoint an entry names is **answering as the instance that
/// entry claims**: a blocking `GET /health` presenting that entry's own token,
/// 2xx only, inside [`DISCOVERY_PROBE_TIMEOUT`].
///
/// Locked decision 30 (#48 F-11 / F-28). Selection used to accept a candidate on
/// the strength of the file alone, so ONE `Write` of a well-formed
/// `.cimp-discovery/<n>.json` naming a deeper root and a dead port steered every
/// child and every hook shim onto an endpoint that could not answer — including
/// the native-web taint beacon, which is fail-open by design and therefore went
/// silent (F-28, decision 14's `sensor` signal).
///
/// **What this does not prove, stated because the decision accepted it.**
/// Liveness proves something is *listening*, not that it is cImp. Whoever can
/// write the discovery file can also bind a loopback port and answer this probe
/// with the token it just wrote. The token check raises that bar a little — the
/// planted entry must name a port the attacker controls, so it cannot borrow the
/// real instance's port and inherit its answer — but the honest bound is: this
/// raises the cost from *one write* to *one write plus a listener*. It does not
/// remove the primitive.
///
/// Blocking on purpose: two of the four consumers (`taint_beacon`,
/// `checkpoint_beacon`) are synchronous shims with no async runtime, and a second
/// async copy of this for the two MCP children is a second thing to get wrong.
/// The callers are all short-lived child processes — no in-app path reaches it
/// (the app's own entry is read pid-keyed by [`read_own_discovery`], which is
/// immune to this primitive and deliberately unprobed).
///
/// **The two MCP children run a `new_current_thread` runtime**, so a probe parks
/// that child's whole runtime — including its `/events` relay task — for as long
/// as it lasts. That is why the budget above is small, why the winner is
/// memoized, and why the worst case is stated in numbers: 1.2 s once per process
/// per hint, against a 10 s heartbeat interval. A refused loopback connect (the
/// ordinary shape of a dead entry) costs microseconds.
pub(super) fn responds(d: &Discovery) -> bool {
    use std::io::{Read, Write};

    let deadline = std::time::Instant::now() + DISCOVERY_PROBE_TIMEOUT;
    // Whatever is left of this candidate's budget. `None` once spent — a zero
    // duration is an error to both `connect_timeout` and `set_*_timeout`, and
    // "no time left" is a non-answer anyway.
    let left = || -> Option<Duration> {
        let now = std::time::Instant::now();
        (deadline > now).then(|| deadline - now)
    };

    let addr = std::net::SocketAddr::from(([127u8, 0, 0, 1], d.port));
    let Some(budget) = left() else { return false };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, budget) else {
        return false;
    };
    let req = format!(
        "GET /health HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {}\r\n\
         Connection: close\r\n\r\n",
        d.token
    );
    let Some(budget) = left() else { return false };
    if stream.set_write_timeout(Some(budget)).is_err() || stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let Some(budget) = left() else { return false };
    if stream.set_read_timeout(Some(budget)).is_err() {
        return false;
    }
    // The status line is the whole answer: `HTTP/1.1 200 OK` from `write_simple`
    // means this instance recognized this entry's token. A 401 (someone else's
    // instance on that port, or a token this instance never issued) and a
    // truncated read are both non-answers.
    let mut head = [0u8; 15];
    let mut filled = 0usize;
    while filled < head.len() {
        match stream.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    let line = String::from_utf8_lossy(&head[..filled]);
    line.strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

/// How many candidates a resolution skipped for not answering — process-local,
/// monotonic, and **surfaced** (not merely counted) by [`proxy_base_for`].
///
/// A skipped candidate is a quality signal and needs a consumer: silently
/// preferring the next entry is right, but a discovery directory that contains a
/// well-formed entry for a port nothing serves is either an unclean shutdown or
/// the F-11 primitive being exercised, and neither should be invisible.
static SKIPPED_CANDIDATES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One-shot budgeted liveness oracle, shared by all three preference steps so
/// the probe ceiling is per RESOLUTION and not per step.
struct Probe<F> {
    alive: F,
    spent: usize,
    skipped: usize,
}

impl<F: FnMut(&Discovery) -> bool> Probe<F> {
    /// Whether this candidate answers. Beyond [`MAX_DISCOVERY_PROBES`] every
    /// candidate reads as a non-answer — the budget is a ceiling on latency, so
    /// exhausting it must not be a way to get an *unverified* entry accepted.
    fn answers(&mut self, d: &Discovery) -> bool {
        if self.spent >= MAX_DISCOVERY_PROBES {
            return false;
        }
        self.spent += 1;
        if (self.alive)(d) {
            true
        } else {
            self.skipped += 1;
            false
        }
    }
}

/// Pick the instance serving `hint` from the per-instance entries: the DEEPEST
/// root that is an ancestor of the hint **and answering** wins (nested checkouts
/// resolve to the closest live instance; same-root duplicates tie-break on pid —
/// arbitrary but deterministic). With no hint or no live match: a sole surviving
/// entry is unambiguous, else fall back to the legacy last-writer-wins file —
/// each of those, too, only if it answers.
fn select_discovery(entries: Vec<Discovery>, hint: Option<&Path>) -> Option<Discovery> {
    select_verified(entries, hint, responds, read_discovery)
}

/// [`select_discovery`] with its **liveness oracle** and its **third**
/// preference injected, so the whole three-step order is unit-testable and not
/// just the first two.
///
/// #48 F-26 is why `legacy` is injected. Two `graph/mcp.rs` comments documented
/// a repro — "truncate `.cimp-discovery/<pid>.json` and the child goes headless"
/// — that does not reproduce, because the entry `read_all_discoveries` silently
/// drops merely takes the caller to step 3, where `.cimp-offload.json` still
/// resolves. That step used to be an inline `read_discovery()` call reading a
/// real file next to the executable, so the one property the wrong comment got
/// wrong was the one property no test could state. `legacy` makes it statable.
///
/// #48 F-11 / F-28 is why `alive` is injected: the ordering and the liveness test
/// are separately assertable, and a test can count probes to pin the budget.
///
/// The order, in one place — every step now **verified** ([`responds`]):
/// 1. among per-instance entries with a non-empty `root` that is an ancestor of
///    (or equal to) the hint, ranked deepest-first with a higher pid breaking a
///    tie, the first that ANSWERS;
/// 2. the sole per-instance entry, when exactly one survives and step 1 did not
///    already rank (and reject) it;
/// 3. `legacy` — in production `.cimp-offload.json` — if it answers.
///
/// Two deliberate non-changes. The deepest-root preference **stays**: dropping it
/// reintroduces "project A's child talks to project B's app", the defect
/// [`DISCOVERY_DIR`] was added to fix. And two *live* entries tying at the same
/// depth still resolve on pid rather than refusing: a live app that legitimately
/// serves the hint is a correct answer either way, and refusing on ambiguity
/// breaks nested checkouts — the very case the per-instance directory exists for.
pub(super) fn select_verified(
    entries: Vec<Discovery>,
    hint: Option<&Path>,
    alive: impl FnMut(&Discovery) -> bool,
    legacy: impl FnOnce() -> Option<Discovery>,
) -> Option<Discovery> {
    let mut probe = Probe {
        alive,
        spent: 0,
        skipped: 0,
    };
    let picked = select_answering(entries, hint, &mut probe, legacy);
    if probe.skipped > 0 {
        SKIPPED_CANDIDATES.fetch_add(probe.skipped, std::sync::atomic::Ordering::Relaxed);
    }
    picked
}

/// The three-step order itself, with the probe budget threaded through. Split
/// from [`select_verified`] only so the skip count is accumulated once, on every
/// exit path, instead of at each `return`.
fn select_answering<F: FnMut(&Discovery) -> bool>(
    mut entries: Vec<Discovery>,
    hint: Option<&Path>,
    probe: &mut Probe<F>,
    legacy: impl FnOnce() -> Option<Discovery>,
) -> Option<Discovery> {
    // Step 1. Rank every matching candidate rather than keeping only the best,
    // because "best" is now "best that answers" and the runner-up has to be
    // reachable. Ranking is unchanged: depth desc, then pid desc.
    let mut ranked: Vec<(usize, &Discovery)> = Vec::new();
    if let Some(h) = hint {
        for d in &entries {
            if d.root.is_empty() {
                continue;
            }
            let root = PathBuf::from(&d.root);
            if is_ancestor_or_equal(&root, h) {
                ranked.push((root.components().count(), d));
            }
        }
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.pid.cmp(&a.1.pid)));
        for (_, d) in &ranked {
            if probe.answers(d) {
                return Some((*d).clone());
            }
        }
    }
    let ranked_any = !ranked.is_empty();

    // Step 2. A sole entry is unambiguous — but only if it answers, and only if
    // step 1 has not already probed it (never probe one candidate twice: the
    // budget is small and a double charge would hide the runner-up).
    if !ranked_any && entries.len() == 1 {
        let sole = entries.pop().expect("len == 1");
        if probe.answers(&sole) {
            return Some(sole);
        }
        // Falling through to the legacy file is new and deliberate: a hard-killed
        // instance leaves its `<pid>.json` behind, so the sole SURVIVING entry can
        // be a dead one while `.cimp-offload.json` names a live instance.
    }

    // Step 3.
    legacy().filter(|d| probe.answers(d))
}

/// Process-local memo of the endpoint each hint resolved to.
///
/// Required by locked decision 30, not an optimization: `proxy_base()` resolves
/// on EVERY tool call, and an unmemoized probe would put a round trip on each
/// one. It is cleared by [`forget_resolved_discovery`] on any failure of the
/// memoized endpoint, which is what preserves the self-healing property this
/// path has today — discovery is re-read per call, so a cImp restart under a live
/// tab recovers instead of wedging that tab headless for life. (Baking
/// port+token into children at spawn was rejected for losing exactly that; see
/// locked decision 30's non-goal.)
///
/// Only successes are memoized. A negative memo would defeat self-healing in the
/// other direction — a child that started before cImp would never find it — and a
/// miss is cheap: a graceful exit removes both stores, so there is nothing to
/// probe.
static RESOLVED: OnceLock<Mutex<HashMap<PathBuf, Discovery>>> = OnceLock::new();

/// Root-aware discovery: resolve the instance serving `hint` (a child's cwd
/// or a hook payload's cwd). `None` hint degrades to sole-entry / legacy.
/// Memoized per hint for the life of the process — see [`RESOLVED`].
pub fn read_discovery_for(hint: Option<&Path>) -> Option<Discovery> {
    let hint = hint.map(canon);
    // `None` and `""` collapse onto one key, which is what they mean here: no
    // project in view. (Empty is not absent — but for a *lookup key* the two are
    // the same lookup, and `select_verified` treats an empty `root` on an ENTRY
    // as unmatchable regardless.)
    let key = hint.clone().unwrap_or_default();
    let memo = RESOLVED.get_or_init(Default::default);
    if let Some(d) = memo
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Some(d.clone());
    }
    let picked = select_discovery(read_all_discoveries(), hint.as_deref())?;
    memo.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, picked.clone());
    Some(picked)
}

/// Drop the memoized endpoints so the next [`read_discovery_for`] re-resolves.
///
/// Every consumer calls this when the endpoint it holds fails, and that is the
/// whole of decision 30's re-resolution half: `offload::mcp::proxy_graph` on any
/// [`ProxyMiss`](crate::offload::mcp), `audit::mcp` when `/audit/run` cannot be
/// reached, and `taint_beacon::dispatch` on a refused connect. Without it the memo could
/// outlive the instance it names — a cImp restart rotates the token and the port,
/// so a stale memo means a permanently headless tab, which is the precise cost
/// that made "bake the endpoint into the child at spawn" a non-goal.
pub fn forget_resolved_discovery() {
    if let Some(memo) = RESOLVED.get() {
        memo.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// Who a stdio MCP child is, as cImp itself composed its argv at spawn.
///
/// A required parameter of [`proxy_base_for`] rather than a module-level
/// `OnceLock` set at child startup, and that is the point: an unset `OnceLock`
/// lets a future caller inherit *"no tab"* by writing nothing — the exact
/// omission shape [`outbound::Flag::attribution`] and `BackendGate::new`'s
/// required positional were both introduced to make impossible. A compile error
/// beats a convention.
///
/// Both fields are cImp-authored (`--consumer` / `--tab`, `tabs/config.rs`) and
/// neither is forgeable *inside the child*. They stop being trustworthy the
/// moment they cross the loopback, which is why the app re-classifies the tab
/// through [`tab_identity`] instead of believing the wire — see
/// [`record_discovery_skipped`].
#[derive(Clone, Copy)]
pub struct ChildIdentity<'a> {
    /// `claude` / `opencode`, as the child was launched.
    pub consumer: &'a str,
    /// The cImp tab this child serves, or `None` for a child spawned without
    /// `--tab` (by hand, or by a pre-V28 cImp).
    pub tab: Option<&'a str>,
}

/// Base URL + bearer token of the loopback endpoint of the instance serving
/// `hint` — the one endpoint resolver every stdio MCP child uses
/// (`offload/mcp.rs`, `audit/mcp.rs`). `None` ⇒ no instance answered.
pub fn proxy_base_for(hint: Option<&Path>, who: ChildIdentity<'_>) -> Option<(String, String)> {
    let d = read_discovery_for(hint);
    // The first consumer for [`SKIPPED_CANDIDATES`], placed here rather than in
    // `read_discovery_for` on purpose: this resolver serves the two stdio MCP
    // children, which already own a stderr diagnostic channel (the handshake
    // lines and `ProxyMiss::report`). The hook shims call `read_discovery_for`
    // directly and must stay on the silent path — `taint_beacon`'s whole safety
    // argument is that it writes nothing to stdout or stderr and awaits nothing.
    report_skipped_candidates();
    let d = d?;
    // #48 F-32 / locked decision 37 — the USER consumer, and it lives here for
    // three reasons that are each load-bearing:
    //
    // 1. **After the `?`.** It fires only when an endpoint resolved, which is
    //    exactly F-32's interesting case: a planted entry was skipped AND the
    //    child reached the real app anyway, so containment worked and nobody was
    //    told. When nothing resolved there is by construction no app to tell,
    //    and stderr stays the only channel — a bound this route's doc states
    //    rather than papers over. (That case is also the loud one: the child is
    //    already degraded through `ProxyMiss::Transport` / `headless_refusal`.)
    // 2. **`d`'s own port and token**, never a re-resolution: no second probe
    //    budget is spent, and the report itself cannot be steered by the very
    //    entry it is reporting.
    // 3. **Still not on the shims' path.** `read_discovery_for` is what
    //    `taint_beacon` and `checkpoint_beacon` call; this function has exactly two
    //    callers, both stdio MCP children. Moving the POST down one frame would
    //    hand `taint_beacon` a write and a wait and destroy locked decision 14's
    //    safety argument — pinned by
    //    `tests::the_discovery_report_never_reaches_the_hook_shims_path`.
    report_skipped_to_app(&d, who, fresh_skips());
    Some((format!("http://127.0.0.1:{}", d.port), d.token))
}

/// Say once, on this child's stderr, that discovery ignored one or more
/// candidate endpoints. Silent when nothing was skipped.
fn report_skipped_candidates() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let n = SKIPPED_CANDIDATES.load(std::sync::atomic::Ordering::Relaxed);
    if n == 0 || SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "cimp: discovery skipped {n} candidate endpoint(s) that did not answer a \
         token-authenticated `GET /health` (#48 F-11, locked decision 30). After an unclean cImp \
         shutdown this is a leftover `.cimp-discovery/<pid>.json` and is harmless. It is ALSO what \
         a planted entry looks like: a well-formed file naming a deeper project root and a port \
         nothing serves is how untrusted content steers this child onto a dead endpoint. If you \
         did not expect it, list `.cimp-discovery/` next to the cImp executable."
    );
}

/// How much of [`SKIPPED_CANDIDATES`] this child has already told the app
/// about, so a later resolution reports only what is NEW.
///
/// Without it the counter's monotonicity would make every resolution after the
/// first restate skips already on record — a row saying "2 candidates were
/// skipped" for a resolution that skipped none. The row must not overstate.
static SKIPS_REPORTED_TO_APP: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Skips this child has not yet reported to the app, marking them reported.
///
/// Deliberately untested in isolation: both counters are process-global and
/// `SKIPPED_CANDIDATES` is written by every `select_verified` in the suite, so a
/// test here would race its neighbours. The COUNT is therefore a parameter of
/// [`report_skipped_to_app`], which is where the decision lives and where the
/// tests are.
fn fresh_skips() -> usize {
    let seen = SKIPPED_CANDIDATES.load(std::sync::atomic::Ordering::Relaxed);
    seen.saturating_sub(SKIPS_REPORTED_TO_APP.swap(seen, std::sync::atomic::Ordering::Relaxed))
}

/// The child's entire network budget for one discovery report, applied to the
/// connect and to the write separately (the response is never read, so there is
/// no third wait).
///
/// The same 80 ms `taint_beacon::DISPATCH_TIMEOUT` uses and for the same reason:
/// on loopback a live app accepts into the backlog immediately and a dead one
/// refuses in microseconds, so this bound is only ever spent against a *wedged*
/// app — the case where waiting would be worst.
///
/// **The arithmetic, stated rather than implied** (decision 30 writes its
/// numbers down for the same reason): the worst case this adds is 160 ms, on a
/// path that already costs up to `MAX_DISCOVERY_PROBES × DISCOVERY_PROBE_TIMEOUT`
/// = **1.2 s** per COLD resolution, against a 10 s heartbeat interval, and runs
/// once per hint per process (the winner is memoized — [`RESOLVED`]). Blocking
/// parks the child's `new_current_thread` runtime exactly as [`responds`] does,
/// and within the bound `responds` already established.
pub(super) const DISCOVERY_REPORT_TIMEOUT: Duration = Duration::from_millis(80);

/// Tell the app that this child skipped `skipped` candidate endpoints —
/// #48 F-32, locked decision 37.
///
/// Silent when nothing was skipped, which is what makes the row mean something:
/// a clean resolution posts nothing at all.
///
/// **No once-per-process flag, and that is the single largest design decision
/// here.** Mirroring [`report_skipped_candidates`]'s `SAID` bit would make the
/// signal fire at most once per child, ever — so its *absence* would read as
/// "nothing was planted" rather than "this was not observable". That is F-24's
/// own defect reappearing inside the fix for its family, and it would be
/// invisible to every test, because tests run in a fresh process.
/// [`forget_resolved_discovery`] is called on *every* endpoint failure, so a
/// child legitimately re-resolves during its life and a second planted entry
/// appearing later must remain reportable.
///
/// Instead the same doubling discipline the app applies
/// ([`outbound::Doubling`]) is applied child-side: reports go out at
/// resolutions-with-skips 1, 2, 4, 8 … so the child's own cost is `log2`-bounded
/// while the signal stays repeatable. The app remains the authority; this only
/// keeps a looping child from being the flood.
pub(super) fn report_skipped_to_app(d: &Discovery, who: ChildIdentity<'_>, skipped: usize) {
    if skipped == 0 {
        return;
    }
    static SENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = SENT
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    if !nth.is_power_of_two() {
        return;
    }
    dispatch_discovery_report(d, who, skipped);
}

/// POST one discovery report and return **without reading the response** —
/// `taint_beacon::dispatch`'s shape, deliberately, and for its reasons.
///
/// Hand-rolled blocking TCP rather than `reqwest`: there is nothing to wait for,
/// and waiting would make this child's latency a function of app health.
/// Every failure — a refused connect, a partial write, a rotated token, a 404
/// from an older app — is swallowed. **Fail-open on reporting; never fail-closed
/// on the child's actual job.** A lost report understates a probe; a blocked
/// tool call breaks a tab, and we never trade the second for the first.
///
/// The body carries only what a child can honestly assert about itself. There is
/// no `cwd`, no path, no free-text field, and no pid/port/root of the skipped
/// entries: those would be attacker-choosable strings presented to an incident
/// reader as forensic fact. The app derives everything else.
pub(super) fn dispatch_discovery_report(d: &Discovery, who: ChildIdentity<'_>, skipped: usize) {
    use std::io::Write;

    let mut body = serde_json::json!({
        "consumer": who.consumer,
        // Clamped on BOTH sides. A single resolution cannot skip more than its
        // probe budget (`Probe::answers`), so the app treats anything above it
        // as definitionally not-a-genuine-child; sending a clamped value keeps
        // the wire honest rather than relying on the far end to fix it.
        "skipped": skipped.min(MAX_DISCOVERY_PROBES),
    });
    // Inserted rather than always-present, so a child with no `--tab` sends a
    // body with no `tab` key at all. `null` and absent read identically to the
    // route, but "the field was never sent" is the more honest wire.
    if let (Some(tab), Some(map)) = (who.tab, body.as_object_mut()) {
        map.insert("tab".to_string(), serde_json::Value::String(tab.to_string()));
    }
    let body = body.to_string();

    let addr = std::net::SocketAddr::from(([127u8, 0, 0, 1], d.port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, DISCOVERY_REPORT_TIMEOUT)
    else {
        // The memoized endpoint answered a probe moments ago and is refusing
        // now: drop it so the child's next call re-resolves rather than
        // inheriting a dead endpoint. Same move `taint_beacon::dispatch` makes.
        forget_resolved_discovery();
        return;
    };
    if stream
        .set_write_timeout(Some(DISCOVERY_REPORT_TIMEOUT))
        .is_err()
    {
        return;
    }
    let req = format!(
        "POST {DISCOVERY_SKIPPED_PATH} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        d.token,
        body.len()
    );
    let _ = stream.write_all(req.as_bytes());
    // No read: the peer's response goes unread and the socket closes on drop.
    // The reply is a fixed `{"ok":true}` on every path anyway — see
    // [`handle_discovery_skipped`], where that is the property, not an accident.
}

/// The path both ends of the discovery report name.
///
/// The dispatch `match` arm has to be a literal (the route-inventory test scans
/// the source for it), so this constant is what the CHILD sends and it is also
/// what `ROUTE_CONTAINMENT` declares — which is what makes the two unable to
/// drift: `no_http_route_can_reach_a_contamination_clear` compares the scanned
/// arm against the declared list, so changing one and not the other fails.
pub(super) const DISCOVERY_SKIPPED_PATH: &str = "/activity/discovery_skipped";

/// Delete per-instance entries whose endpoint no longer answers — hard-killed
/// instances leave their `<pid>.json` behind (removal is graceful-exit only).
/// Run once per instance start; a 200ms connect probe per entry bounds the
/// cost. Entries for OUR pid are removed unconditionally (a previous run's
/// leftover under a reused pid — ours gets rewritten right after).
pub(super) async fn sweep_stale_discoveries(own_pid: u32) {
    for d in read_all_discoveries() {
        let stale = if d.pid == own_pid {
            true
        } else {
            !tokio::time::timeout(
                Duration::from_millis(200),
                tokio::net::TcpStream::connect(("127.0.0.1", d.port)),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        };
        if stale {
            let path = discovery_dir().join(format!("{}.json", d.pid));
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    debug!(error = %e, pid = d.pid, "offload loopback: stale discovery cleanup failed");
                }
            } else if d.pid != own_pid {
                info!(
                    pid = d.pid,
                    port = d.port,
                    "offload loopback: swept stale discovery entry"
                );
            }
        }
    }
}
