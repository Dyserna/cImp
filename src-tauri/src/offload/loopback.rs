//! V8-03 loopback proxy channel — the authenticated localhost endpoint the
//! per-session `--offload-mcp` child forwards to when the app is running.
//!
//! A minimal hand-rolled HTTP/1.1 service on `127.0.0.1:0` (ephemeral port),
//! gated by a per-launch bearer token. It exposes exactly three routes —
//! purpose-built for offload, **not** a general local API:
//!
//! - `POST /run` — run one `offload_task` against the warm app-side pool.
//! - `GET  /describe` — the live capability description for the tool.
//! - `GET  /events` — an SSE stream the child relays to Claude: capability
//!   pulses (`event: change` → `notifications/tools/list_changed`) and, since
//!   V30 Phase B, addressed session pushes (`event: push` →
//!   `notifications/claude/channel`).
//!
//! (It has grown a few more since — the graph/audit/context/memory routes the
//! per-tab child and the harness hooks post to, plus `GET /status`, the V32
//! Phase B taint-latch debug view.)
//!
//! The `{port, token, pid}` are advertised in a discovery file written next
//! to the exe (the portable root — never `~/.claude`), created when offload
//! is enabled and removed on exit. The token rotates every launch. This is
//! the one genuinely new security surface: loopback-only bind + token auth +
//! a user-readable discovery file (tightened where the OS allows). A
//! malicious *local* process that reads the file could drive offloads or
//! observe task text — the same localhost-dev-server trust assumption,
//! documented in MAINTENANCE.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};

use super::agent::ThinkingMode;
use super::mcp_host::Consumer;
use super::router::TierHint;
use super::service::{OffloadService, PushNotice};
use super::detection;
use super::outbound::{self, Budget};
use super::toolclass::{self, Latch, Profile, ProxyGate, ToolClass, WriteTaint};

/// Discovery-file name under the portable root (next to `settings.json`).
/// Legacy single-instance location, still written for anything that only
/// knows this path; the per-instance directory below is authoritative.
const DISCOVERY_FILE: &str = ".cimp-offload.json";

/// Per-instance discovery DIRECTORY under the portable root: one
/// `<pid>.json` per running instance, each carrying that instance's launch
/// `root`. The legacy single file is last-writer-wins, so with two cImp
/// instances open a child spawned by project A's agent could connect to
/// project B's app — and audits/graph queries would run against the WRONG
/// project. Readers resolve root-aware via [`read_discovery_for`].
const DISCOVERY_DIR: &str = ".cimp-discovery";

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
fn own_discovery_path() -> PathBuf {
    discovery_dir().join(format!("{}.json", std::process::id()))
}

/// Read the legacy single discovery file, if present and parseable.
pub fn read_discovery() -> Option<Discovery> {
    let text = std::fs::read_to_string(discovery_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every parseable per-instance discovery entry (stale ones included — they
/// are swept at instance start; see [`sweep_stale_discoveries`]).
fn read_all_discoveries() -> Vec<Discovery> {
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
fn canon(p: &Path) -> PathBuf {
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
fn is_ancestor_or_equal(root: &Path, hint: &Path) -> bool {
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
/// Blocking on purpose: two of the four consumers (`context_hook`,
/// `taint_beacon`) are synchronous shims with no async runtime, and a second
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
fn responds(d: &Discovery) -> bool {
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
fn select_verified(
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
/// reached, `context_hook::post_loopback` on a failed post and
/// `taint_beacon::dispatch` on a refused connect. Without it the memo could
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
    //    `context_hook` and `taint_beacon` call; this function has exactly two
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
const DISCOVERY_REPORT_TIMEOUT: Duration = Duration::from_millis(80);

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
fn report_skipped_to_app(d: &Discovery, who: ChildIdentity<'_>, skipped: usize) {
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
fn dispatch_discovery_report(d: &Discovery, who: ChildIdentity<'_>, skipped: usize) {
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
const DISCOVERY_SKIPPED_PATH: &str = "/activity/discovery_skipped";

/// Delete per-instance entries whose endpoint no longer answers — hard-killed
/// instances leave their `<pid>.json` behind (removal is graceful-exit only).
/// Run once per instance start; a 200ms connect probe per entry bounds the
/// cost. Entries for OUR pid are removed unconditionally (a previous run's
/// leftover under a reused pid — ours gets rewritten right after).
async fn sweep_stale_discoveries(own_pid: u32) {
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

/// A per-launch random bearer token (two v4 UUIDs of entropy, hex). Avoids
/// pulling a separate RNG crate — `uuid` is already a dependency.
fn make_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// A running loopback endpoint. Holds the port/token and the discovery path
/// so [`Self::stop`] can remove the file on exit. `port`/`token` round out
/// the handle for diagnostics + any future direct status read.
pub struct Loopback {
    #[allow(dead_code)]
    pub port: u16,
    #[allow(dead_code)]
    pub token: String,
    discovery: PathBuf,
    /// This instance's `<pid>.json` under the per-instance directory.
    own_file: PathBuf,
}

impl Loopback {
    /// Bind the endpoint, write the discovery files (the per-instance
    /// `<pid>.json` plus the legacy single file), sweep stale sibling
    /// entries, and spawn the accept loop. Returns the handle (managed in
    /// `AppState`). Idempotent at the file level — existing (stale) files
    /// are overwritten. `root` is the launch project root this instance
    /// serves; children match their cwd against it (`read_discovery_for`).
    pub async fn start(
        service: Arc<OffloadService>,
        app: AppHandle,
        root: &Path,
    ) -> AppResult<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| AppError::Offload(format!("loopback bind failed: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| AppError::Offload(format!("loopback addr failed: {e}")))?
            .port();
        let token = make_token();
        let discovery = discovery_path();
        let own_file = own_discovery_path();

        sweep_stale_discoveries(std::process::id()).await;

        let disc = Discovery {
            port,
            token: token.clone(),
            pid: std::process::id(),
            root: canon(root).to_string_lossy().to_string(),
        };
        if let Some(parent) = own_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_discovery(&own_file, &disc)?;
        // Keep the legacy single file in step (last-writer-wins, as before):
        // it is the no-hint / no-per-instance-match fallback.
        write_discovery(&discovery, &disc)?;
        info!(port, root = %disc.root, "offload loopback: listening on 127.0.0.1");

        // Accept loop.
        let accept_token = token.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let svc = service.clone();
                        let app = app.clone();
                        let tok = accept_token.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = handle_conn(stream, svc, app, tok).await {
                                debug!(error = %e, "offload loopback: connection ended");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "offload loopback: accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            port,
            token,
            discovery,
            own_file,
        }))
    }

    /// Remove the discovery file (graceful exit / disable). The accept task
    /// is detached and dies with the process.
    ///
    /// Only removes the file if it still belongs to *this* process. The
    /// discovery path is shared per exe-dir, so a second app instance can
    /// overwrite it with its own port/token/pid; deleting that on our exit
    /// would leave the surviving instance undiscoverable to its offload
    /// children. (The start-time clobber itself is inherent to running two
    /// instances from one install and is left as last-writer-wins.)
    pub fn stop(&self) {
        // The per-instance file is ours by construction (pid-named).
        if let Err(e) = std::fs::remove_file(&self.own_file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(error = %e, "offload loopback: own discovery cleanup failed");
            }
        }
        if let Some(d) = read_discovery() {
            if d.pid != std::process::id() {
                debug!(
                    owner_pid = d.pid,
                    "offload loopback: discovery file owned by another instance; not removing"
                );
                return;
            }
        }
        if let Err(e) = std::fs::remove_file(&self.discovery) {
            if e.kind() != std::io::ErrorKind::NotFound {
                debug!(error = %e, "offload loopback: discovery cleanup failed");
            }
        }
    }
}

/// Write the discovery file, tightening permissions to user-only where the
/// OS supports it (best-effort).
fn write_discovery(path: &PathBuf, disc: &Discovery) -> AppResult<()> {
    let body = serde_json::to_string(disc)
        .map_err(|e| AppError::Offload(format!("discovery serialize: {e}")))?;
    std::fs::write(path, body).map_err(|e| AppError::Offload(format!("discovery write: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// A `POST /run` request body.
#[derive(Deserialize)]
struct RunBody {
    instructions: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    /// The calling session's working directory (the repo Claude Code runs in),
    /// used as the native-tool root when no explicit `allowed_roots` is set.
    #[serde(default)]
    cwd: Option<String>,
    /// V21 F9: optional JSON Schema — when set, the worker's final answer is
    /// grammar-constrained to matching JSON. Absent on legacy child requests.
    #[serde(default)]
    schema: Option<serde_json::Value>,
    /// V32 Phase A: optional task shape (`"research"` | `"code"`) that
    /// pre-applies the worker's taint latch. Kept as a raw string on the wire
    /// and re-validated here (see [`handle_run`]) rather than deserialized into
    /// the enum, so an invalid value produces the tool-facing error message
    /// instead of a generic serde "bad request body".
    #[serde(default)]
    profile: Option<String>,
    /// V32 C-1c (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --offload-mcp --tab <id>`), resolved to the tab's taint latch.
    /// Absent on a legacy child ⇒ the fail-open anonymous scope.
    #[serde(default)]
    tab: Option<String>,
    /// V32 C-1c: which agent is calling (`claude` / `opencode`, from the
    /// child's `--consumer` flag), sent in the body exactly as `/graph_run`
    /// does. The latch registry is keyed by `(agent, tab)`, so a missing
    /// consumer would key an OpenCode tab's calls under the Claude agent and
    /// gate against a latch that is not the caller's. Absent ⇒ `claude`, the
    /// route's long-standing default.
    #[serde(default)]
    consumer: Option<String>,
    /// V32 C-1c: which of the two offload tools the caller invoked, so the
    /// refusal and its activity row name the tool the model actually called.
    /// An `offload_batch` fans out to one `/run` per subtask, so this route
    /// serves both. Validated against the two known names at the parse boundary
    /// ([`offload_tool_name`]) rather than trusted — it reaches an activity row.
    #[serde(default)]
    tool: Option<String>,
}

/// The offload tool name a `/run` body names, defaulted and validated (C-1c).
///
/// Anything other than the two real names — absent, a legacy child that sends
/// no `tool`, or an invented string — reads as `offload_task`. This is a
/// *labelling* input, not a capability one: both names classify
/// LOCAL-CAPABILITY, so no value can change the gate's verdict, and pinning the
/// vocabulary keeps a caller from choosing what an activity row says.
fn offload_tool_name(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some("offload_batch") => "offload_batch",
        _ => "offload_task",
    }
}

/// A `POST /run` response.
#[derive(Serialize)]
struct RunResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Parse one NDJSON line of a loopback-streamed run body (`/run`,
/// `/audit/run`) from the child's side — kept beside [`RunResult`] so the
/// encoder and the decoder of the wire shape live in one file. The final
/// result line is the ONLY one carrying an `ok` boolean; every other line —
/// heartbeats of any shape, blanks, unparseable bytes — yields `None`, so the
/// heartbeat wire format is not load-bearing. `fallback_error` fills in when
/// an `ok:false` line carries no `error` text.
pub fn parse_result_line(raw: &[u8], fallback_error: &str) -> Option<Result<String, String>> {
    let line = std::str::from_utf8(raw).ok()?.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ok = v.get("ok").and_then(|b| b.as_bool())?;
    if ok {
        Some(Ok(v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()))
    } else {
        Some(Err(v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or(fallback_error)
            .to_string()))
    }
}

/// The stream head shared by the NDJSON-streaming routes (`/run`,
/// `/audit/run`): no `Content-Length` — the body is close-delimited — sent up
/// front so the child's `send()` resolves immediately and it knows the app is
/// alive before the (possibly long) task even starts.
const NDJSON_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Content-Type: application/x-ndjson\r\n\
    Cache-Control: no-cache\r\n\
    Connection: close\r\n\r\n";

/// One heartbeat line of an NDJSON-streamed run body. Carries no `ok` field,
/// so [`parse_result_line`] skips it; the exact shape is not load-bearing.
const HEARTBEAT_LINE: &[u8] = b"{\"hb\":true}\n";

/// Write the shared [`NDJSON_HEAD`] stream head.
async fn write_ndjson_head<W>(wr: &mut W, label: &str) -> AppResult<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    wr.write_all(NDJSON_HEAD)
        .await
        .map_err(|e| AppError::Offload(format!("{label} head: {e}")))?;
    wr.flush().await.ok();
    Ok(())
}

/// Serialize + write the final [`RunResult`] line: one JSON object (serde
/// emits no embedded newlines) + `\n`, then the caller lets the connection
/// close. This is the single `ok`-bearing line [`parse_result_line`] keys off.
async fn write_result_line<W>(wr: &mut W, r: &RunResult, label: &str) -> AppResult<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_vec(r)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"failed to serialize result"}"#.to_vec());
    line.push(b'\n');
    wr.write_all(&line)
        .await
        .map_err(|e| AppError::Offload(format!("{label} result: {e}")))?;
    wr.flush().await.ok();
    Ok(())
}

/// A parsed HTTP request: method, path, headers (lowercased keys), and body.
struct Request {
    method: String,
    path: String,
    auth: Option<String>,
    body: Vec<u8>,
}

/// Read and parse one HTTP/1.1 request from the stream (headers + an
/// optional Content-Length body). Bounded so a malformed client can't make
/// us read forever.
async fn read_request(stream: &mut TcpStream) -> AppResult<Request> {
    const MAX_HEADER: usize = 16 * 1024;
    const MAX_BODY: usize = 4 * 1024 * 1024;
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];

    // Read until the header terminator.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER {
            return Err(AppError::Offload("request headers too large".into()));
        }
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| AppError::Offload(format!("read failed: {e}")))?;
        if n == 0 {
            return Err(AppError::Offload("connection closed before headers".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut auth = None;
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim();
            match key.as_str() {
                "authorization" => auth = Some(val.to_string()),
                "content-length" => content_length = val.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    if content_length > MAX_BODY {
        return Err(AppError::Offload("request body too large".into()));
    }

    // Body bytes already buffered past the header terminator, plus any more.
    let mut body: Vec<u8> = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| AppError::Offload(format!("read body failed: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    Ok(Request {
        method,
        path,
        auth,
        body,
    })
}

/// Find the first index of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Whether the request carries the expected bearer token. Uses a constant-time
/// comparison so a local attacker can't recover the per-launch token byte by
/// byte from 401-response timing (best-effort; the length check leaks only the
/// fixed token length).
fn authorized(req: &Request, token: &str) -> bool {
    match &req.auth {
        Some(h) => h
            .strip_prefix("Bearer ")
            .map(|t| ct_eq(t.as_bytes(), token.as_bytes()))
            .unwrap_or(false),
        None => false,
    }
}

/// Constant-time byte-slice equality: compares all bytes of equal-length inputs
/// without short-circuiting on the first mismatch.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Handle one connection: route by method+path after checking auth.
async fn handle_conn(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
    app: AppHandle,
    token: String,
) -> AppResult<()> {
    // Cap how long we'll wait for a complete request: a half-open or idle
    // connection (TCP probe, crashed child holding the socket) would otherwise
    // wedge this handler task forever, leaking one task per such connection.
    let req = match tokio::time::timeout(Duration::from_secs(30), read_request(&mut stream)).await {
        Ok(r) => r?,
        Err(_) => return Err(AppError::Offload("request read timed out".into())),
    };

    if !authorized(&req, &token) {
        write_simple(&mut stream, 401, "text/plain", b"unauthorized").await?;
        return Ok(());
    }

    // Match on the path without its query string (`/mcp/list?consumer=opencode`
    // must route the same as `/mcp/list`); handlers read the query themselves.
    let route = req.path.split('?').next().unwrap_or(&req.path);
    // V35 Phase I: observe the CHP protocol version this caller speaks, BEFORE
    // dispatch and beside it rather than inside nine handlers — the routes' own
    // body types stay byte-identical, which is this phase's exit criterion. It
    // reads only, answers nothing and cannot reject: a route's behaviour is the
    // same whether this line runs or not.
    note_chp(&app, route, &req.body);
    match (req.method.as_str(), route) {
        ("POST", "/run") => handle_run(&mut stream, &service, &app, &req).await,
        ("POST", "/graph_run") => handle_graph_run(&mut stream, &app, &req).await,
        ("POST", "/audit/run") => handle_audit_run(&mut stream, &app, &req).await,
        ("POST", "/context/retrieve") => handle_context_retrieve(&mut stream, &app, &req).await,
        ("POST", "/workbench/tool_checkpoint") => handle_tool_checkpoint(&mut stream, &app, &req).await,
        ("POST", "/context/compaction") => handle_context_compaction(&mut stream, &app, &req).await,
        ("POST", "/context/should_read") => handle_should_read(&mut stream, &app, &req).await,
        ("POST", "/context/post_edit") => handle_post_edit(&mut stream, &app, &req).await,
        ("POST", "/memory/event") => handle_memory_event(&mut stream, &app, &req).await,
        ("POST", "/activity/contract_drift") => handle_contract_drift(&mut stream, &req).await,
        ("POST", "/activity/discovery_skipped") => handle_discovery_skipped(&mut stream, &app, &req).await,
        ("POST", "/permission/event") => handle_permission_event(&mut stream, &app, &req).await,
        ("POST", "/latch/beacon") => handle_latch_beacon(&mut stream, &app, &req).await,
        ("POST", "/latch/state") => handle_latch_state(&mut stream, &app, &req).await,
        ("POST", "/session/hello") => handle_session_hello(&mut stream, &app, &req).await,
        // NOTE (#45): there is deliberately no `POST /latch/override`. The
        // manual override is a capability GRANT, and the bearer token gating
        // this listener is readable by every process running as the user, so an
        // HTTP door onto it made the latch model-movable and its audit row a
        // lie. The `latch_override` IPC command is the only way in; falling
        // through to the 404 below is the intended behaviour for anything that
        // still tries this path.
        ("POST", "/mcp/list") => handle_mcp_list(&mut stream, &service, &req).await,
        ("POST", "/mcp/call") => handle_mcp_call(&mut stream, &service, &app, &req).await,
        ("GET", "/describe") => {
            let text = service.describe().await;
            write_simple(
                &mut stream,
                200,
                "text/plain; charset=utf-8",
                text.as_bytes(),
            )
            .await
        }
        ("GET", "/events") => handle_events(stream, service, &req).await,
        ("GET", "/health") => write_simple(&mut stream, 200, "text/plain", b"ok").await,
        ("GET", "/status") => handle_status(&mut stream, &app).await,
        _ => write_simple(&mut stream, 404, "text/plain", b"not found").await,
    }
}

/// How often the streamed `/run` response emits a heartbeat line while the task
/// is still running. The proxy treats a gap several times this long as "the
/// worker wedged" (see `mcp.rs`), so this must stay well under that idle window.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// `POST /run`: decode the task, run it on the warm pool, and **stream** the
/// result as newline-delimited JSON — periodic `{"hb":true}` heartbeats while
/// the (possibly minutes-long) task runs, then a final `{"ok":..}` line.
///
/// The heartbeats are the whole point of this being a stream: they let the
/// proxy distinguish a slow-but-alive job (keep waiting) from a dead app
/// (fall back), so a long run is never abandoned and re-executed. The response
/// has no `Content-Length`; the body is delimited by connection close.
///
/// **Taint gate (V32 C-1c, 2026-08-07 review).** `offload_task`/`offload_batch`
/// were TRUSTED, waved through on the rationale that "the delegated subtask gets
/// its own latch". It does — a *fresh and permissive* one:
/// `Latch::from_profile(task.profile)`, and `Profile::Code.latch()` is
/// `Latch::Local`, which **grants** `read_file`/`code_search`/`run_command`,
/// exactly the class a latched caller just lost. An OpenCode tab with the Phase
/// H native gate on, contaminated by a `webfetch`, could call
/// `offload_task { profile: "code", instructions: "print the contents of .env" }`
/// and get the file's text back as an ordinary tool result — with no
/// spotlighting envelope, no detection scan and no budget charge, since all
/// three are `/mcp/call`-only — then carry it out through `webfetch`. Phase H
/// bypassed end to end.
///
/// The demotion to LOCAL-CAPABILITY is the decision; this is where it binds,
/// because this route is the only one both tools reach. Decision 4 is untouched:
/// the *declared profile* still pre-applies the sub-task's own latch, which is
/// about the sub-task's shape, not about whether the caller may delegate at all.
async fn handle_run(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: RunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    if body.instructions.trim().is_empty() {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some("`instructions` must be non-empty".into()),
        };
        return write_json(stream, 400, &r).await;
    }

    let thinking = ThinkingMode::parse(body.thinking.as_deref().unwrap_or("auto"));
    let tier = TierHint::parse(body.tier.as_deref().unwrap_or("auto"));

    // V32 Phase A: validate `profile` at this parse boundary. Unlike
    // thinking/tier (benign fallbacks), an unrecognized profile must NOT
    // silently degrade to "no containment" — the MCP schema's `enum` is an
    // upstream guarantee, and upstream guarantees get re-checked post-hoc.
    let profile = match body.profile.as_deref() {
        None => None,
        Some(raw) => match Profile::parse(raw) {
            Ok(p) => Some(p),
            Err(msg) => {
                let r = RunResult {
                    ok: false,
                    text: None,
                    error: Some(msg),
                };
                return write_json(stream, 400, &r).await;
            }
        },
    };

    // V32 C-1c: the taint gate, after every parse-boundary rejection so a
    // malformed request never engages a latch, and before any work starts.
    // ONE settings read for identity + policy; an unknown tab id yields no
    // scope and keys no registry entry (#45's bound, via the same `latch_scope`
    // funnel). `LatchRoute::Native` — this route serves cImp's own tools, never
    // a proxied server's content.
    let tool = offload_tool_name(body.tool.as_deref());
    let settings = live_settings(app);
    let scoping = latch_scope(
        app,
        &settings,
        crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or("claude")),
        body.tab.as_deref(),
    );
    if let LatchScoping::Unknown(tab) = &scoping {
        warn!(
            target: "offload",
            tab = %tab,
            tool = %tool,
            "loopback: /run has no configured tab to latch against — delegation is ungated"
        );
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(&settings, scope);
    // `CallProvenance::internal()`: cImp's own dispatch, and a native route
    // serves no fetched page — there is no content origin to name (#48, F-3).
    if let Err(refusal) = latches().gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        // 200 with `ok:false`: a task-level error the child renders as a tool
        // result, the same framing `/run`'s own failures use. Sent before
        // `write_ndjson_head`, so a plain single-JSON body — which the child's
        // reader handles as the unterminated trailing line.
        return write_json(stream, 200, &r).await;
    }

    let session_cwd = body.cwd.map(std::path::PathBuf::from);
    // V33 Phase F: the requesting tab, for the pre-mutation checkpoint the
    // worker takes before `run_command`. Read BEFORE the `service.run` call
    // below, which consumes the rest of `body`, and narrowed through the SAME
    // `tab_identity` funnel `/context/retrieve`'s prompt-tap checkpoint uses
    // (V33 C5) — an id naming no configured tab of this consumer is a forged or
    // stale claim, and a checkpoint is the one record that exists to be trusted
    // after an incident, so it degrades to "cannot attribute" rather than to
    // "some other tab".
    let checkpoint_tab = match tab_identity(
        &settings,
        crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or("claude")),
        body.tab.as_deref(),
    ) {
        TabIdentity::Configured(t) => Some(t.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };

    // Cancellation: trip the token if the calling client disconnects while the
    // task runs, so the in-flight chat stream is dropped and llama-server frees
    // the slot instead of finishing an orphaned generation. After the request
    // body a well-behaved client (reqwest, in the MCP child) sends nothing and
    // does NOT half-close its write half until it has the response — so a probe
    // read returning 0 bytes (EOF) means the whole connection went away.
    let cancel = CancellationToken::new();
    let run_fut = service.run(
        body.instructions,
        body.context,
        thinking,
        tier,
        session_cwd,
        checkpoint_tab.as_deref(),
        body.schema,
        profile,
        cancel.clone(),
    );
    tokio::pin!(run_fut);

    // Split so heartbeats/result (write half) and the disconnect probe (read
    // half) can run concurrently on the one connection.
    let (mut rd, mut wr) = stream.split();
    write_ndjson_head(&mut wr, "run").await?;

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await; // consume the immediate first tick

    let result = loop {
        let mut probe = [0u8; 1];
        tokio::select! {
            biased;
            r = &mut run_fut => break r,
            // Check for a caller disconnect *before* the heartbeat branch: a
            // clean FIN should cancel promptly, not wait out a heartbeat write
            // that still succeeds (the FIN is on the read half) and holds the
            // slot for up to one HEARTBEAT_INTERVAL longer.
            read = rd.read(&mut probe) => match read {
                Ok(0) | Err(_) => {
                    debug!("offload loopback: caller disconnected mid-task; cancelling");
                    cancel.cancel();
                    break (&mut run_fut).await;
                }
                // A stray byte before the response is unexpected on this
                // one-shot protocol; ignore it and keep waiting.
                Ok(_) => continue,
            },
            _ = beat.tick() => {
                // A failed heartbeat write means the client went away; cancel
                // and let the task unwind (its stream drop frees the slot).
                if wr.write_all(HEARTBEAT_LINE).await.is_err() {
                    debug!("offload loopback: heartbeat write failed; caller gone, cancelling");
                    cancel.cancel();
                    break (&mut run_fut).await;
                }
                wr.flush().await.ok();
            }
        }
    };
    let r = match result {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        // `ok:false` is a task-level error the child renders as a tool result
        // so Claude can read + adapt.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e.to_string()),
        },
    };
    write_result_line(&mut wr, &r, "run").await
}

/// A `POST /graph_run` request body (the warm code-graph query path).
#[derive(Deserialize)]
struct GraphRunBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (same ancestor-walk the MCP child uses).
    #[serde(default)]
    cwd: Option<String>,
    /// The `graph_*` / `context_*` tool name.
    name: String,
    /// The tool arguments.
    #[serde(default)]
    args: Value,
    /// The requesting consumer (`"claude"` / `"opencode"`); selects the activity
    /// source and the `context_*` tools' per-agent session scope. Defaults to
    /// Claude when absent.
    #[serde(default)]
    consumer: Option<String>,
    /// V28 (issue #13): the cImp TAB id the calling MCP child was spawned for
    /// (`cimp --offload-mcp --tab <id>`), used to resolve *which* session of
    /// this agent the `context_*` memory tools should scope to. Optional by
    /// design — a child spawned before the upgrade sends no `tab`, and an
    /// unknown/stale one resolves to `None`; both fall back to the pre-V28
    /// most-recent-session behavior rather than erroring the call.
    #[serde(default)]
    tab: Option<String>,
}

/// A `POST /mcp/call` request body (a Claude-exposed MCP tool invocation).
#[derive(Deserialize)]
struct McpCallBody {
    /// The namespaced `<server>__<tool>` name.
    name: String,
    /// The tool arguments.
    #[serde(default)]
    arguments: Value,
    /// The calling session's working directory (the child's cwd), used to
    /// attribute the Tool Activity row to a project. Optional by design — a
    /// child from before this field sends none, and the row just gets an
    /// empty root.
    #[serde(default)]
    cwd: Option<String>,
    /// V32 Phase B: the cImp TAB id the calling MCP child was spawned for.
    /// V28 sent `tab` on `/graph_run` only — external servers hold no cImp
    /// memory scope — but the taint latch needs the *same* identity on both
    /// tool-serving routes or a tab could launder an external fetch past its
    /// own latch. Optional on exactly the same fail-open terms as
    /// [`GraphRunBody::tab`].
    #[serde(default)]
    tab: Option<String>,
}

// ── V32 Phase B — the consumer-side taint latch ────────────────────────────

/// The identity one gated call carries: which agent, which tab, and which
/// session that tab is currently running.
///
/// `agent` is always the normalized `claude`/`opencode` vocabulary
/// ([`crate::graph::source_for_consumer`]) because the two gated routes learn
/// the consumer differently — `/graph_run` from the body, `/mcp/call` from the
/// `?consumer=` query — and one tab MUST key identically from either, or its
/// web fetches and its graph reads would latch two separate scopes.
#[derive(Debug, PartialEq, Eq)]
struct LatchScope {
    agent: &'static str,
    tab: String,
    /// The session the V28 registry currently reports for this tab, or `None`
    /// when it withholds one (no live entry yet, TTL-stale, or the H1
    /// same-root ambiguity). `None` is *absence of evidence*, never evidence of
    /// a new session — see [`TabLatch::observe`].
    session: Option<String>,
    /// The project root this tab runs against, in [`crate::activity::root_key`]
    /// form — the `root` column of the activity rows this scope's calls
    /// produce (#48, finding F-3).
    ///
    /// It rides on the scope rather than on each call because it is a property
    /// of the tab, and because [`latch_scope`] is the ONE funnel every gated
    /// route resolves identity through: a row written from a path that has a
    /// scope therefore cannot be written without a root, which is the mistake
    /// `beacon_row` and the memory-quarantine row made (`root: ""`, so neither
    /// can be filtered per project, so neither can appear on a per-project
    /// surface).
    ///
    /// **Resolved from settings, not from the request.** The gated bodies all
    /// carry a `cwd`, but that is the calling child's claim about itself; the
    /// tab id is config-derived and validated against the same snapshot
    /// ([`is_configured_tab`]), so `crate::tabs::ai_tab_dir` gives a root with
    /// the same trust level as the identity it hangs off. Empty only if no
    /// working directory can be resolved at all — see [`tab_root_key`].
    root: String,
}

impl LatchScope {
    /// The registry key. Tuple, not a formatted string, so no tab id
    /// containing the separator could collide with another agent's tab.
    fn key(&self) -> (&'static str, String) {
        (self.agent, self.tab.clone())
    }

    /// V32 Phase G: this scope as the injection resolver addresses it. The two
    /// vocabularies are deliberately the same pair (`agent`, `tab`) so a tab's
    /// latch and a tab's override row can never key differently.
    fn injection(&self) -> crate::settings::injection::Scope<'_> {
        crate::settings::injection::Scope::Tab {
            agent: self.agent,
            tab: &self.tab,
        }
    }

    /// The human-readable scope label carried by V32 `injection_flag` activity
    /// rows. Formatted (unlike [`key`](Self::key)) because it is for a reader,
    /// not for equality.
    fn label(&self) -> String {
        format!("{}:{}", self.agent, self.tab)
    }

    /// This scope as an activity-row attribution (#48 F-29).
    ///
    /// A scope exists only for a **configured** tab id ([`is_configured_tab`],
    /// checked by [`latch_scope`]), so [`Attribution::Tab`] is a fact here — the
    /// same reading [`LatchScoping::attribution`] takes for its `Scoped` arm,
    /// which now delegates here so the two cannot drift.
    ///
    /// The tab id is taken from the field, never re-split out of
    /// [`label`](Self::label): a round trip through a formatted string is how
    /// `"{agent}:(no tab identity)"` became a *tab named `(no tab identity)`*
    /// once already (`outbound::scope_attribution`'s doc).
    fn attribution(&self) -> crate::activity::Attribution {
        crate::activity::Attribution::Tab(self.tab.clone())
    }
}

/// Whether `tab` names an AI tab the **user has configured for `agent`** (#45;
/// consumer-scoped by V33 C5, finding F-4).
///
/// This is the predicate that makes [`latches`]' "bounded by construction"
/// claim true rather than aspirational. Every registry entry is keyed on a
/// tab id that arrives in a request body, so without this the map's key space
/// is "whatever a caller typed" — no TTL, no cap, no eviction, and every entry
/// serialized into every `/status` response and every 4 s `latch_status` poll.
/// With it, the key space is a subset of the user's own tab list.
///
/// **V33 C5 — the pair, not the halves.** Until V33 this asked only "is this
/// *some* configured AI tab id", while every registry key is the PAIR
/// `(agent, tab)` ([`LatchScope::key`]) and `agent` is caller-asserted on every
/// route that has one. A caller could therefore key a latch under
/// `("claude", <an OpenCode tab's id>)` and the pair was verified on no route in
/// the system. It is now verified here, at the one funnel
/// ([`latch_scope`]) every entry-creating path resolves through: the id must
/// name a configured tab **of the asserted consumer**, classified by
/// [`crate::tabs::tab_consumer`] — the same call the launch path makes when it
/// decides what to inject into that tab, so the two ends cannot drift.
///
/// **Not a live exploit today, a restored invariant.** The V32 review rated the
/// cross-keyed case harmless on the routes that exist: a latch keyed under the
/// wrong agent is freshly open, engages a scope nobody reads, and refuses
/// nothing. What it bought was a registry key space twice the size of the tab
/// list and a `(consumer, tab)` pair no route checked.
///
/// **The check is still "is this a configured tab id of this consumer", NOT "is
/// this the tab that owns this connection".** The stricter form would break
/// legitimate beacons today: the OpenCode plugin file is written per *directory*
/// (one file per tab since #48's H-2 fix, but every tab in a directory still
/// loads every file), so the tab id baked into it may belong to a different tab
/// sharing the same working dir. Whoever fixes H-2's remainder may tighten this;
/// until then, binding a beacon to its connection would reject real beacons from
/// real tabs.
///
/// **`AiTool` tabs only.** Shell and Preview tabs host no harness, so nothing
/// legitimate can beacon or gate as one.
///
/// **The empty-list escape**, and why it is keyed on the WHOLE list rather than
/// on this consumer's slice. With no AI tab configured at all the predicate
/// accepts everything, because [`live_settings`] falls back to
/// `Settings::default()` (whose `tabs` is empty) when managed state is not up
/// yet — and a request arriving in that window must not be rejected on the
/// strength of a list we could not read. That condition is "settings are
/// unreadable", which is global; narrowing the *floor* to "this consumer has no
/// tabs" would have widened it instead, handing every forged id a scope on any
/// install that runs only Claude tabs or only OpenCode ones — i.e. re-opening
/// exactly the unbounded key space #45 closed. So the floor keeps its original
/// trigger and only the positive test is consumer-scoped, which makes this
/// change a strict tightening of the admitted set.
fn is_configured_tab(settings: &crate::settings::Settings, agent: &'static str, tab: &str) -> bool {
    names_a_configured_ai_tab_for(settings, agent, tab) || ai_tab_ids(settings).next().is_none()
}

/// Every configured AI tab's id, in settings order — **every consumer's**.
///
/// Two callers, and neither is the latch: [`names_a_configured_ai_tab`] (a
/// collision check across the whole id space) and [`is_configured_tab`]'s
/// availability floor (whose condition is "settings are unreadable", not
/// "this consumer has no tabs"). Identity checks use
/// [`ai_tab_ids_for`] instead.
fn ai_tab_ids(settings: &crate::settings::Settings) -> impl Iterator<Item = &str> {
    settings.tabs.iter().filter_map(|t| match t {
        crate::settings::TabConfig::AiTool(c) => Some(c.id.as_str()),
        _ => None,
    })
}

/// Every configured AI tab id belonging to `agent` (`"claude"` / `"opencode"`),
/// in settings order — V33 C5's key space.
fn ai_tab_ids_for<'a>(
    settings: &'a crate::settings::Settings,
    agent: &'static str,
) -> impl Iterator<Item = &'a str> {
    settings.tabs.iter().filter_map(move |t| match t {
        crate::settings::TabConfig::AiTool(c) if crate::tabs::tab_consumer(c) == agent => {
            Some(c.id.as_str())
        }
        _ => None,
    })
}

/// Whether `id` exactly names a configured AI tab **of `agent`** —
/// [`is_configured_tab`] without its availability floor.
fn names_a_configured_ai_tab_for(
    settings: &crate::settings::Settings,
    agent: &'static str,
    id: &str,
) -> bool {
    ai_tab_ids_for(settings, agent).any(|t| t == id)
}

/// Whether `id` **exactly** names a configured AI tab of ANY consumer —
/// [`is_configured_tab`] without its availability floor and without its
/// consumer scope.
///
/// The floor is an availability floor for the *latch* (a gate that rejects
/// every id before settings load would refuse real tool calls). It is the wrong
/// polarity for a caller that must be REFUSED for naming a tab, where "no tabs
/// configured yet" must mean "this string collides with nothing" — so that
/// caller gets this predicate instead of a negated one. See
/// [`mark_live_session_from_event`], its only consumer.
///
/// V33 C5 left the consumer scope off this one deliberately, for the same
/// polarity reason: it asks "does this session id collide with a TAB id", and a
/// collision is a collision whichever consumer owns the tab. Narrowing it to one
/// consumer would let an OpenCode session id equal to a Claude tab id through.
fn names_a_configured_ai_tab(settings: &crate::settings::Settings, id: &str) -> bool {
    ai_tab_ids(settings).any(|t| t == id)
}

/// Which of three cases a request body's `(agent, tab)` falls into, decided
/// **without** the `AppHandle` [`latch_scope`]'s session lookup needs.
///
/// V33 C5: `agent` is part of the question, not context carried alongside it —
/// `Configured` now means "a tab of THIS consumer", which is what the registry
/// key `(agent, tab)` has always asserted and nothing checked.
///
/// Split out (#48) for two reasons. It is the enforcement point for the
/// registry bound, and a bound asserted by calling [`is_configured_tab`] beside
/// `latch_scope` rather than through it survives deleting the call from
/// `latch_scope` — which is what
/// `tests::only_configured_ai_tab_ids_can_ever_key_a_latch` did. And it names
/// the distinction #45 collapsed: "no tab id" and "an id that names no
/// configured tab" are the same for the *registry* and not the same for a
/// *verdict*.
#[derive(Debug, PartialEq, Eq)]
enum TabIdentity<'a> {
    /// No `tab` at all (absent, empty, or whitespace) — a child spawned before
    /// `--tab` existed.
    Anonymous,
    /// A non-empty id naming no configured AI tab. The trimmed id is carried
    /// for the log lines and error messages that have to quote it back.
    Unknown(&'a str),
    /// A configured AI tab id, trimmed.
    Configured(&'a str),
}

fn tab_identity<'a>(
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&'a str>,
) -> TabIdentity<'a> {
    let Some(tab) = tab.map(str::trim).filter(|t| !t.is_empty()) else {
        return TabIdentity::Anonymous;
    };
    if is_configured_tab(settings, agent, tab) {
        TabIdentity::Configured(tab)
    } else {
        TabIdentity::Unknown(tab)
    }
}

/// Resolve the calling tab's latch scope, keeping the two identity-less cases
/// apart (#48).
///
/// [`LatchScoping::scope`] is `None` for both, which is the **fail-open** case
/// (locked, and V28's existing discipline): a child spawned before `--tab`
/// existed sends nothing, and a tool call must never fail for lack of identity.
/// It is deliberately NOT promoted to a global latch — one identityless call
/// would then latch every consumer at once. Such calls still get the
/// spotlighting envelope on EXTERNAL results, which needs no identity.
///
/// #45 widened "no identity" to include **an id that is not a configured tab**,
/// and V33 C5 widened it again to **an id that is not a configured tab of the
/// asserted consumer** ([`is_configured_tab`]). This is the single funnel every entry-creating path
/// resolves through — `/graph_run` and `/mcp/call` via `gate`, `/latch/beacon`
/// via `beacon` — so validating here is what bounds the registry, rather than
/// three route-local checks that can drift apart. An unknown id creates no row
/// and gates nothing; a caller that invents ids only ever talks to a scope that
/// does not exist, which is where it started. **That part is unchanged.**
///
/// What #48 changes is only that the two cases are now *distinguishable* by the
/// caller. Folding them into one `Option::None` also folded them into
/// `handle_latch_state`'s hard-off verdict, which was a Phase H regression: see
/// [`LatchScoping::Unknown`].
///
/// Takes the settings snapshot rather than reading its own, so a handler
/// resolves identity and policy under the SAME snapshot (the "ONE settings read
/// for the whole call" discipline `/mcp/call` documents).
fn latch_scope(
    app: &AppHandle,
    settings: &crate::settings::Settings,
    agent: &'static str,
    tab: Option<&str>,
) -> LatchScoping {
    match tab_identity(settings, agent, tab) {
        TabIdentity::Anonymous => LatchScoping::Anonymous,
        TabIdentity::Unknown(tab) => LatchScoping::Unknown(tab.to_string()),
        TabIdentity::Configured(tab) => {
            let session = app
                .try_state::<Arc<crate::graph::GraphService>>()
                .and_then(|g| g.live_session_for_tab(tab, agent));
            LatchScoping::Scoped(LatchScope {
                agent,
                tab: tab.to_string(),
                session,
                root: tab_root_key(app, settings, tab),
            })
        }
    }
}

/// The project root one configured AI tab runs against, as an activity
/// `root_key` (#48, finding F-3). See [`LatchScope::root`] for why the tab —
/// rather than the request body's `cwd` — is the source.
///
/// Two fallbacks, in order, and both are deliberate:
///
/// 1. **The app's launch directory**, when the id resolves to no AI tab config.
///    That is reachable through [`is_configured_tab`]'s empty-list escape (a
///    request that arrives before managed state is up), and the launch dir is
///    what such a tab *would* run in — [`crate::tabs::ai_tab_dir`] returns the
///    per-tab `cwd` override or exactly this.
/// 2. **The process cwd**, when managed state is not up at all. The app sets
///    `LaunchContext::cwd` from the process cwd at startup and never chdirs, so
///    this is the same directory by another route rather than a guess.
///
/// An empty string is possible only if even `current_dir()` fails (a deleted
/// cwd). It is not papered over with a placeholder: a root that cannot be
/// resolved must read as absent, not as some other project.
fn tab_root_key(app: &AppHandle, settings: &crate::settings::Settings, tab: &str) -> String {
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return String::new();
    };
    let dir = crate::tabs::ai_tab_dir(settings, tab, &launch).unwrap_or(launch);
    crate::activity::root_key(&dir)
}

/// The outcome of [`latch_scope`] — [`TabIdentity`] with the session folded in.
#[derive(Debug, PartialEq, Eq)]
enum LatchScoping {
    /// No tab identity at all. Fail-open everywhere.
    Anonymous,
    /// An id that names no configured AI tab — a forged one, or (the case that
    /// makes this worth a variant) a **stale real one**: the OpenCode plugin is
    /// written per working *directory* with one tab id baked in (the unfixed
    /// H-2), so removing or re-id'ing that tab leaves the file on disk still
    /// naming an id the settings no longer have.
    ///
    /// It keys no registry entry — that is #45's bound and it is untouched —
    /// but it must not be read as "containment is off" either, because the two
    /// look identical to the plugin and only one of them is a decision anyone
    /// took. See `handle_latch_state`.
    Unknown(String),
    /// A configured AI tab. The only variant that can key the registry.
    Scoped(LatchScope),
}

impl LatchScoping {
    /// The scope, when there is one. `None` for both identity-less variants —
    /// the fail-open reading `gate`, `beacon` and `budget_gate` take, and the
    /// reason an unknown id still creates no registry entry.
    fn scope(&self) -> Option<&LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// Consume into the scope, for the callers that need to keep it.
    fn into_scope(self) -> Option<LatchScope> {
        match self {
            LatchScoping::Scoped(s) => Some(s),
            _ => None,
        }
    }

    /// #51 / #48 F-20 — which tab an activity row written for this call belongs
    /// to.
    ///
    /// The mapping is one-to-one onto [`crate::activity::Attribution`], and that
    /// is the whole point: both enums were derived from the same three facts — no
    /// tab identity at all / an id naming no configured tab / a configured tab —
    /// and the row's column exists to report which of the three this call was.
    /// Written here, once, so `/graph_run` and `/mcp/call` cannot answer it
    /// differently, and so a future route gets the answer by resolving identity
    /// rather than by remembering to.
    ///
    /// Both handlers used to call [`Self::into_scope`] immediately, which
    /// collapses `Anonymous` and `Unknown` into one `None`. That collapse is
    /// right for the latch (both fail open) and wrong for the row, which has to
    /// keep them apart — that collapse IS finding F-20.
    ///
    /// **Not `Attribution::from_child_argv`.** A tab id that reached this frame
    /// came out of a request BODY, which a caller can invent; the argv
    /// constructor's own doc forbids it here. [`latch_scope`] has already run the
    /// id through [`is_configured_tab`], and `Unrecognized` is what the
    /// unvalidated case is called on the row.
    ///
    /// **#48 F-39 / locked decision 42 — that invented id is BOUNDED here.** It
    /// is an arbitrary-length string from a request body and it lands in a row's
    /// attribution column, in a **capped per-lane ring**: a caller choosing how
    /// many bytes one row occupies is choosing how much of the lane it fills, and
    /// the rows that fall out the other end are the genuine ones. Same
    /// consequence as F-37 by a different route, and the same cure F-32 already
    /// left in the tree — [`bounded_id`], **applied AFTER classification**, which
    /// is the load-bearing half: [`latch_scope`] resolved this variant by running
    /// the FULL string through [`is_configured_tab`], so no truncated invented id
    /// can ever fold onto a configured one. Bounding at the parse boundary
    /// instead would close a bloat hole by opening an impersonation hole.
    ///
    /// [`Attribution::Unattributed`](crate::activity::Attribution::Unattributed)
    /// is deliberately unreachable from this function: a route that resolved a
    /// `LatchScoping` at all DOES know, and "the writer does not know" would be a
    /// false claim — the one thing that column must never make.
    fn attribution(&self) -> crate::activity::Attribution {
        match self {
            LatchScoping::Anonymous => crate::activity::Attribution::Headless,
            LatchScoping::Unknown(id) => {
                crate::activity::Attribution::Unrecognized(bounded_id(id))
            }
            LatchScoping::Scoped(s) => s.attribution(),
        }
    }

    /// The injection-hierarchy scope this call resolves features against.
    /// Both identity-less variants resolve as an **unknown caller**
    /// (`Scope::UnknownCaller`), the same fail-open reading
    /// [`GatePolicy::resolve`] has always taken for a scope-less call: the
    /// app-wide answer is the honest floor when there is no tab to ask about,
    /// and it is what an unrecognized id resolved to before #45 (L3 not found ⇒
    /// `Inherit` ⇒ L2 ⇒ L1).
    ///
    /// #48 F-35: that variant was called `Scope::App` until locked decision 36
    /// split it in two. Behaviour is unchanged — this site was always asking the
    /// identity-less question, and it keeps N-1's elevation (any configured
    /// tab's L3 `On` is honoured, because the caller IS one of those tabs).
    fn injection(&self) -> crate::settings::injection::Scope<'_> {
        self.scope().map_or(
            crate::settings::injection::Scope::UnknownCaller,
            LatchScope::injection,
        )
    }
}

/// One tab's latch, together with the session identity it was engaged for.
struct TabLatch {
    session: Option<String>,
    latch: Latch,
    /// V32 Phase C: this session's EXTERNAL call/byte spend (locked decision
    /// 11). It lives *here*, beside the latch, precisely so it inherits the
    /// latch's scope and reset rule — one conversation, one budget, both
    /// cleared together when the tab's session rotates. (H-2: `contaminated`
    /// no longer rides along; a permissive reset and an un-tainting reset need
    /// different evidence — see [`TabLatch::contaminated`].)
    budget: Budget,
    /// Whether this session's taint-latch refusal has already been reported to
    /// the Tool Activity feed. One row per scope: the latch is sticky, so every
    /// later refusal restates the same fact.
    latch_flagged: bool,
    /// Whether this session's native-web BEACON has already been reported
    /// (#48). Same one-row-per-scope bound and the same reset as
    /// [`latch_flagged`](Self::latch_flagged), and it exists for the same
    /// reason: a caller that POSTs `/latch/beacon` in a loop must produce one
    /// row, not one per request, or it floods a capped feed and evicts the rows
    /// the audit trail exists to keep.
    ///
    /// It is a separate bit from "the latch moved" because #45 keyed the row on
    /// the latch transition alone, and a beacon can change this conversation's
    /// state **without** moving the latch: a tab already latched `Local`
    /// (Phase A's other direction) takes the contamination bit and keeps its
    /// latch, which then quarantines every later `context_note` — silently,
    /// under the old condition.
    beacon_flagged: bool,
    /// V32 Phase F (locked decision 15): whether external content has entered
    /// this conversation *at all* — set the moment an EXTERNAL call is admitted
    /// (proxied, or beaconed from a harness-native web tool) and cleared by
    /// exactly three USER actions, none of them reachable over HTTP (see "Step
    /// 4" and "The 2026-08-10 amendment" below).
    ///
    /// It exists because decision 15 lets the USER move the latch, and the two
    /// facts then come apart: the latch says what the session may do NEXT,
    /// while contamination says what is already in its context window. A note
    /// written after "switch to local" was still composed by a model that read
    /// an attacker's page, so persistence must stay quarantined — contamination
    /// is a property of the conversation, not of the latch position.
    ///
    /// That argument governs [`LatchOverride::FlipLocal`] and only it. The
    /// 2026-08-10 amendment below draws the line where the user's own click
    /// draws it: a *workflow step* cannot un-taint a conversation, but a
    /// deliberate return to FULL access — the strictly larger risk — is the
    /// verdict the flip is not.
    ///
    /// # H-2 (2026-08-08 re-review): the bit is STICKY, and why
    ///
    /// It used to be cleared by [`TabLatch::observe`] on a session rotation,
    /// "because only a new conversation has a clean context". The premise was
    /// wrong about *who decides* a rotation happened. A Claude tab's session id
    /// is the file stem of the newest `*.jsonl` under its transcript root, and
    /// **decision 3 puts Claude's native Bash outside every cImp latch** — so
    /// the model can create that file itself. Every bar the tap could put in
    /// front of the rotation (the file exists / the file grew / the file
    /// decodes as a transcript naming that session) is a bar over a file the
    /// attacker is writing. C-2 fixed "the file exists", H-2 defeated "the file
    /// grew" with one trailing newline, and "it decodes" falls to `echo
    /// '{"sessionId":"…"}'`. **No filesystem-derived fact can be the trust root
    /// for un-tainting a context window**, so the reset is gone rather than
    /// re-armed.
    ///
    /// This amends locked decision 15: contamination is now a property of the
    /// **tab**, not of the conversation, deliberately — because the conversation
    /// boundary is attacker-controlled and the tab id is not (it is
    /// config-derived, and [`is_configured_tab`] bounds the key space).
    ///
    /// # Step 4 (2026-08-09): the clear path H-2 left open, and its trust root
    ///
    /// H-2 removed the last automatic reset and left "restart cImp" as the only
    /// escape — which is why the field doc used to end there. What it did not
    /// settle is what *may* clear the bit, and the answer is not a better piece
    /// of evidence: it is **authority**. A human acting in cImp's own UI is a
    /// fact no shell can fabricate (the webview holds no bearer token, and
    /// [`apply_latch_override`] is reachable only from the capability-scoped
    /// `latch_override` IPC command), and it is the same trust root every other
    /// consent surface in this app already uses.
    ///
    /// So exactly three things clear it, all rooted in that click:
    ///
    /// 1. [`LatchOverride::ClearContamination`] — the user judged the flagged
    ///    content harmless. Cleared immediately; nothing else about the tab or
    ///    its session changes.
    /// 2. [`LatchOverride::AwaitSessionClear`] +
    ///    [`awaiting_session_clear`](Self::awaiting_session_clear) — the user
    ///    restored a checkpoint. The bit **stays set** and lifts only when a
    ///    proved session rotation is observed. See that field for why a forgeable
    ///    rotation signal is acceptable *there* and nowhere else.
    /// 3. [`LatchOverride::Unlatch`] — the user restored FULL access. See "The
    ///    2026-08-10 amendment" below for the argument.
    ///
    /// **The accepted cost is unchanged for everything else.** A genuine
    /// `/clear` in a tab nobody armed keeps the bit: that conversation's
    /// `context_note` writes stay quarantined (they are stored and held for
    /// review, not dropped) and the badge keeps saying "contaminated".
    /// [`LatchOverride::FlipLocal`] — the workflow flip — still cannot clear it,
    /// and neither can any HTTP route: `/latch/beacon` only ever tightens, and
    /// `POST /latch/override` has not existed since #45.
    ///
    /// # The 2026-08-10 amendment: a full unlatch IS a verdict
    ///
    /// **The 2026-08-10 amendment to decision 15** (user: *"if the user restores
    /// full access then the tab should be cleared, it's the user's decision."*).
    /// A full unlatch hands back read AND web with the injected content still in
    /// the context window; that is the strictly larger risk, and it is taken
    /// behind the popover's own confirmation. Leaving the *memory* half
    /// quarantined after it made the product overrule a judgement it had just
    /// asked the user to make. Same trust root as (1): authority, not evidence.
    ///
    /// **Clearing the STATE never erases the EVIDENCE.** The
    /// [`outbound::Screen::Contamination`] row that set the bit stays in its own
    /// retention lane, and every release writes an
    /// [`outbound::Screen::ContaminationCleared`] row beside it — including the
    /// unlatch's ([`unlatch_clear_row`]). "Cleared" and "never contaminated" are
    /// therefore distinguishable in the feed even though the live view is
    /// identical, which is the point.
    contaminated: bool,
    /// Step 4: the **one-shot arm** — the only thing that lets a session
    /// rotation clear [`contaminated`](Self::contaminated).
    ///
    /// Set by [`LatchOverride::AwaitSessionClear`], i.e. by a user who restored
    /// a checkpoint. Consumed by [`observe`](Self::observe) the next time a
    /// changed session id arrives, which clears the contamination bit and
    /// disarms in the same move. Also cleared by an immediate
    /// [`LatchOverride::ClearContamination`], which supersedes it (there is
    /// nothing left to wait for).
    ///
    /// # Why restore does not simply clear, and why the arm is safe
    ///
    /// **Restore is the case where clearing is *least* justified.** Rolling back
    /// files cannot remove injected text from the model's context window, so the
    /// conversation the user is worried about is still running. The UI therefore
    /// tells them to `/clear`, and cImp waits until it sees that happen.
    ///
    /// **And this is the one place a filesystem-derived rotation signal may be
    /// trusted.** H-2's argument is intact: a Claude tab's session id comes from
    /// a directory the model's own Bash can write, so the signal is forgeable.
    /// What changes here is what the signal *decides*. It is not carrying the
    /// decision — the click is. An attacker cannot click restore, so a forged
    /// rotation only helps in the case where the user has **already decided** the
    /// bit should go; the worst outcome is that it lifts slightly earlier than
    /// their actual `/clear`. The signal answers "has the authorised thing
    /// happened yet?", never "should it happen?". H-2's decode proof still gates
    /// it: `observe` only ever sees session ids the live-session registry
    /// published, and that registry takes Claude ids from
    /// `oob::claude::LiveSessionGate` (a decoded record naming the session) and
    /// OpenCode ids from a `session.created` on the harness's own event stream.
    ///
    /// # Lifetime
    ///
    /// It lives in the registry entry, so it has the entry's lifetime: it
    /// survives a tab restart (which is itself a rotation, and therefore fires
    /// it) and dies with the process. An app restart drops the whole entry —
    /// contamination included — so an arm outliving one is meaningless by
    /// construction.
    awaiting_session_clear: bool,
    /// #48 (F-23): this tab's `local` latch was put there by the USER's
    /// [`LatchOverride::FlipLocal`] click — it was **not** earned by a
    /// local-capability tool call.
    ///
    /// # Why the position alone is not enough
    ///
    /// `Latch::Local` has two causes and the refusals for them are different
    /// statements. Reached by [`Latch::engage`] it means *"this session read a
    /// file, so the web side closed"*; reached by decision 15's workflow flip it
    /// means *"a human closed the web side and handed local capability back"*.
    /// The web-direction refusal used to serve the first sentence in both cases
    /// ([`toolclass::REFUSAL_NATIVE_WEB_BLOCKED`]), which is F-23: a refusal
    /// stating a cause it did not check. The gate cannot recover that cause from
    /// [`Latch`] — the enum is a position, not a history — so the fact is
    /// recorded here at the one site that performs the flip, beside the
    /// [`outbound::Screen::LatchOverride`] row that records the same act for the
    /// audit trail.
    ///
    /// # Lifetime, and why it cannot outlive its latch
    ///
    /// Set **only** in `apply_override`'s [`LatchOverride::FlipLocal`] arm, and
    /// cleared everywhere the latch leaves `Local`: the [`LatchOverride::Unlatch`]
    /// arm and [`observe`](Self::observe)'s rotation reset. Those are the only
    /// three writes to [`latch`](Self::latch) in this module, so the field cannot
    /// describe a latch position that is no longer in force — a stale `true` on a
    /// re-latched `local` would attribute a tool call's latch to the user, which
    /// is F-23 with the operands swapped.
    ///
    /// It is deliberately **not** a "the user touched this tab" flag: `Unlatch`,
    /// `ClearContamination` and `AwaitSessionClear` are user actions too and none
    /// of them leaves the latch `local`. What this answers is exactly one
    /// question — *why is the web side closed?* — and it is read by exactly one
    /// consumer, the native-web direction of the Phase H gate, through
    /// [`LatchView::local_by_user_flip`].
    local_by_user_flip: bool,
}

impl TabLatch {
    /// A brand-new, uncontaminated entry. One constructor so a field added
    /// later cannot be initialized two different ways at the two sites that
    /// create rows (`gate` and the Phase F `beacon`).
    fn fresh() -> Self {
        TabLatch {
            session: None,
            latch: Latch::Open,
            budget: Budget::default(),
            latch_flagged: false,
            beacon_flagged: false,
            contaminated: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }

    /// The user-facing projection of this entry (Phase F): what the badge
    /// shows and which override buttons the popover may enable.
    fn view(&self) -> LatchView {
        LatchView {
            latch: self.latch.label(),
            contaminated: self.contaminated,
            // Decision 15's two moves, as availability rather than as UI
            // knowledge: the frontend must not re-derive "when is flip legal"
            // from the label, or the rule would live in two places and drift.
            can_flip_local: self.latch == Latch::External,
            can_unlatch: self.latch != Latch::Open,
            // Step 4's two, published on the same principle. `can_clear` is
            // deliberately not `contaminated` spelled twice in TypeScript: the
            // legality rule for a move belongs to the backend even when it is
            // currently one field wide.
            can_clear: self.contaminated,
            awaiting_session_clear: self.awaiting_session_clear,
            // F-23: published rather than re-derived, for the same reason
            // `can_flip_local` is — a consumer that inferred "the user must have
            // flipped it" from `latch == "local" && contaminated` would be
            // guessing, and would be wrong for the tab that fetched a page,
            // latched EXTERNAL and was never flipped at all.
            local_by_user_flip: self.local_by_user_flip,
        }
    }

    /// Step 4: **the one place [`contaminated`](Self::contaminated) is
    /// cleared.** Returns what the tab looked like immediately before, or `None`
    /// when it was not contaminated at all.
    ///
    /// All three authorised paths funnel through here — the user's immediate
    /// resume, the full unlatch (2026-08-10 amendment) and the armed rotation —
    /// so a field that has to be reset alongside the bit cannot be reset on one
    /// path and forgotten on the other.
    ///
    /// # What it resets, and why the two report bits are not optional
    ///
    /// `latch_flagged` and `beacon_flagged` are one-row-per-scope claim bits:
    /// once set, the refusal and beacon rows they gate are never written again
    /// for this tab-session. Leaving them set across a clear would make a
    /// **re-contamination silent** — the tab would take external content again
    /// and the feed would show nothing new, which is exactly the class of bug
    /// #48 fixed for the `Local`-latched beacon. Clearing the bit means this tab
    /// gets to report its containment events afresh.
    ///
    /// (The `contamination` row itself is self-limiting through
    /// [`note_contamination`]'s `mem::replace`, so clearing the bit re-arms that
    /// one automatically — which is what the re-contamination test asserts,
    /// rather than asserting these two booleans directly.)
    ///
    /// # What it deliberately does NOT touch
    ///
    /// * **The latch.** Resume "changes nothing else"; the latch has its own two
    ///   buttons and its own rules. Leaving it where it is can only be the
    ///   tighter choice. (The unlatch path moves the latch too — but it does so
    ///   in its own arm of [`LatchRegistry::apply_override`], *after* this
    ///   function has run, so `PriorTaint::latch` still names the latch the bit
    ///   was released from. This function never touches the latch on any path.)
    /// * **The budget.** Spend is not a report flag — the same reason
    ///   [`LatchRegistry::apply_override`] does not refill it.
    /// * **Quarantined notes.** Locked decision 10 keeps promote-or-discard
    ///   behind the Memory view's own review, which is a separate consent
    ///   surface. Clearing the tab bit stops *future* writes being quarantined;
    ///   notes already held stay held. Nothing in this module can reach them, and
    ///   that is the point.
    fn clear_contamination(&mut self) -> Option<PriorTaint> {
        if !self.contaminated {
            // An arm can only be set on a contaminated tab, but if one ever
            // outlived its bit it would be a trap waiting to fire on the next
            // rotation. Drop it.
            self.awaiting_session_clear = false;
            return None;
        }
        let prior = PriorTaint {
            latch: self.latch.label(),
            armed: self.awaiting_session_clear,
            session: self.session.clone(),
        };
        self.contaminated = false;
        self.awaiting_session_clear = false;
        self.latch_flagged = false;
        self.beacon_flagged = false;
        Some(prior)
    }

    /// Fold the currently-observed session id into this entry, resetting the
    /// latch when the tab's session has demonstrably **rotated**.
    ///
    /// This is what makes the LATCH's scope "the tab's live session" rather
    /// than "the tab": a tab restart starts a new harness session, the V28
    /// registry re-stamps the tab with the new id, and the new conversation is
    /// not denied `read`/`bash` on the strength of the previous one's fetch.
    /// (The tab id itself never rotates — it is config-derived — so keying on
    /// it alone would strand a tab latched until the app restarted.)
    ///
    /// **What a rotation does NOT do (H-2).** It does not clear
    /// [`contaminated`](Self::contaminated). Everything reset here is
    /// permissive state that the next real call re-earns; the contamination bit
    /// is the one fact whose reset an attacker would *want*, and the rotation
    /// signal is derived from a file that attacker can create. See the field's
    /// doc for the full argument and its accepted cost.
    ///
    /// **The one exception, and it is not an exception to that argument (step
    /// 4).** If — and only if — the user has armed
    /// [`awaiting_session_clear`](Self::awaiting_session_clear) by restoring a
    /// checkpoint, this rotation is the awaited event and the bit is cleared
    /// here. H-2 is untouched: an unarmed tab is unchanged by any rotation,
    /// forged or genuine, which is what
    /// `a_session_rotation_resets_the_latch_but_never_the_contamination_bit`
    /// still asserts. The arm is the authority; the rotation only says "yet".
    ///
    /// The three cases, and why `None` is not one of them:
    /// - a *different* session id ⇒ new scope, latch back to [`Latch::Open`];
    /// - the *same* id ⇒ unchanged;
    /// - **no** id ⇒ unchanged, and the stored id is kept. The registry
    ///   withholds a session for reasons that have nothing to do with restarts
    ///   (TTL staleness, the H1 same-root ambiguity, a tab that has not yet
    ///   emitted a session-bearing event). Treating that silence as a restart
    ///   would hand an injected model a trivial latch reset: keep calling until
    ///   the registry blinks.
    #[must_use = "an armed tab clears its contamination bit here and the clear owes the feed a \
                  row — call ContaminationCleared::record() after dropping the registry lock"]
    fn observe(&mut self, session: Option<&str>) -> Option<ClearedOnRotation> {
        let s = session?;
        match self.session.as_deref() {
            Some(prev) if prev == s => None,
            Some(prev) => {
                let prior_session = prev.to_string();
                // Captured before the resets below, because it is what the audit
                // row means by "prior state".
                let prior_latch = self.latch.label();
                self.session = Some(s.to_string());
                self.latch = Latch::Open;
                // #48 (F-23): the latch this described is gone, so the reason it
                // was in that position goes with it. Left set, a `local` latch
                // re-earned by the next conversation's file read would be
                // reported as the user's decision.
                self.local_by_user_flip = false;
                // V32 Phase C: the new conversation gets a fresh budget and a
                // fresh right to report — same scope, same reset.
                self.budget.reset();
                self.latch_flagged = false;
                self.beacon_flagged = false;
                // H-2 (2026-08-08 re-review): `contaminated` is deliberately
                // NOT cleared here — see the field's own doc. A rotation is a
                // claim about a file an attacker can create; it may reopen the
                // latch and refill the budget (both merely permissive, and both
                // re-earned by the next real call), but it may not un-taint a
                // context window.
                //
                // Step 4: unless the USER armed this exact wait. The guard is
                // the whole design — it is checked before anything is cleared,
                // so a rotation into an unarmed tab takes the H-2 path above and
                // nothing else. Deliberately not `if let Some(..) = ..` over
                // `clear_contamination`: that call must not run at all on an
                // unarmed tab, or a later refactor of it could start reaching
                // the bit through this door.
                if !self.awaiting_session_clear {
                    return None;
                }
                let prior = self.clear_contamination()?;
                Some(ClearedOnRotation {
                    prior_latch,
                    prior_session,
                    session: s.to_string(),
                    armed: prior.armed,
                })
            }
            // First sighting: the same scope, only now identified. The latch
            // carries over — calls made before the registry knew the session
            // still happened in this conversation.
            //
            // Not a rotation, so it cannot fire the arm either: "we did not know
            // the id before" is not evidence that the conversation changed. This
            // is the same reading `None` gets above.
            None => {
                self.session = Some(s.to_string());
                None
            }
        }
    }
}

/// What a tab looked like immediately before [`TabLatch::clear_contamination`]
/// released it — the "prior state" every clear's audit row records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorTaint {
    /// [`Latch::label`] at the moment of the clear — i.e. the latch the bit was
    /// released *from*. `clear_contamination` never changes it; on the unlatch
    /// path the caller moves the latch to `Open` immediately afterwards, so this
    /// reads `external`/`local`, which is the state an audit row means by
    /// "prior". It therefore always equals `OverrideOutcome::prior.label()`.
    latch: &'static str,
    /// Whether the one-shot arm was set. False for a false-positive resume of an
    /// un-armed tab; true when the clear is a restore's arm firing.
    armed: bool,
    /// The conversation the tab was in. For the rotation path this is the
    /// *outgoing* session — the contaminated one.
    session: Option<String>,
}

/// A contamination bit cleared inside [`TabLatch::observe`] — the armed
/// one-shot firing on a proved session rotation.
///
/// Returned rather than recorded in place for the reason [`Contamination`] is:
/// the transition happens under the registry mutex and `record_flag` does file
/// I/O. Every caller of `observe` turns this into a
/// [`ContaminationCleared`] with its own [`LatchScope`] — which is also why the
/// scope is not carried here.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a contamination clear that is not recorded is an unaudited release of containment"]
struct ClearedOnRotation {
    /// [`Latch::label`] before the rotation reopened it.
    prior_latch: &'static str,
    /// The contaminated conversation that just ended.
    prior_session: String,
    /// The conversation the tab is now in.
    session: String,
    /// Always true — the arm is the only way to reach this type. Carried so the
    /// row builder takes the same `armed` input on both paths.
    armed: bool,
}

/// V32 Phase F: the containment state of one tab, as the badge and the
/// override popover need it. Shared by `/status`, the `latch_status` IPC
/// command and the two Phase F endpoints' replies so all four describe a tab
/// with the same four facts.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatchView {
    /// [`Latch::label`]: `open` / `external` / `local`.
    pub latch: &'static str,
    /// Locked decision 15: has external content entered this conversation at
    /// all? Survives [`LatchOverride::FlipLocal`] and — since H-2 — every
    /// *unarmed* session rotation: the bit is sticky for the tab's registry
    /// entry. It is released only by the three USER actions listed on
    /// [`TabLatch::contaminated`], one of which is
    /// [`LatchOverride::Unlatch`] (2026-08-10 amendment).
    pub contaminated: bool,
    /// Whether "switch to local" applies right now (EXTERNAL-latched only).
    pub can_flip_local: bool,
    /// Whether "restore full access" applies right now (anything but open).
    pub can_unlatch: bool,
    /// Step 4: whether either contamination clear applies right now — i.e.
    /// whether the tab is contaminated at all.
    pub can_clear: bool,
    /// Step 4: whether the user has armed the one-shot clear (they restored a
    /// checkpoint) and cImp is waiting for this tab to start a new harness
    /// session. See [`TabLatch::awaiting_session_clear`].
    ///
    /// Published because the UI has to say *why* a contaminated tab is showing
    /// no "clear now" affordance after a restore, and because step 5's
    /// restore-linked entry point must be able to tell an already-armed tab from
    /// a fresh one without re-deriving the rule.
    pub awaiting_session_clear: bool,
    /// #48 (F-23): whether [`latch`](Self::latch) reads `local` because the USER
    /// flipped it there, rather than because a local-capability tool ran. See
    /// [`TabLatch::local_by_user_flip`].
    ///
    /// Published on `/latch/state` so the OpenCode plugin's web-direction refusal
    /// can serve the constant whose cause it actually checked. It is a fact cImp
    /// recorded when it applied the override, so selecting a message with it is a
    /// lookup — not a message composed from anything a caller supplied.
    ///
    /// `false` for every latch that is not `local`, by construction: the three
    /// writes to the underlying latch keep the two in step.
    pub local_by_user_flip: bool,
}

impl Default for LatchView {
    /// The view of a tab the proxy has never served: nothing latched, nothing
    /// contaminated, no override available.
    fn default() -> Self {
        LatchView {
            latch: Latch::Open.label(),
            contaminated: false,
            can_flip_local: false,
            can_unlatch: false,
            can_clear: false,
            awaiting_session_clear: false,
            local_by_user_flip: false,
        }
    }
}

/// The USER-initiated containment moves — V32 Phase F's two latch moves
/// (locked decision 15), plus step 4's two contamination moves.
///
/// There is still no "latch external": the system does that, and an action that
/// only ever tightens needs no consent surface. What step 4 adds is the *clear*
/// this enum's doc used to say could not exist. H-2's conclusion was that no
/// filesystem-derived **evidence** may un-taint a context window, and that
/// stands; the trust root here is not evidence but **authority** — a human
/// acting in cImp's own UI (see [`TabLatch::contaminated`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatchOverride {
    /// EXTERNAL → Local. Restores the proxied local-capability tools and closes
    /// the external side in the same move.
    FlipLocal,
    /// Anything → Open. Restores both sides; the UI puts a confirmation in
    /// front of it because it recreates the trifecta with injected content
    /// still in the context window.
    ///
    /// **And it clears [`TabLatch::contaminated`]** — decision 15's 2026-08-10
    /// amendment. The click is a verdict, not a workflow step: it already hands
    /// back the strictly more dangerous capability, so quarantining persistent
    /// memory afterwards overruled the user's own decision. On an
    /// uncontaminated tab it clears nothing and is still legal; the clear is a
    /// consequence of the action, not its precondition (contrast
    /// [`ClearContamination`](Self::ClearContamination), whose entire purpose is
    /// the clear, so "nothing to clear" is *its* error).
    Unlatch,
    /// Step 4 — **false-positive resume.** The user has looked at what was
    /// flagged and judged it harmless. Clears the contamination bit now; the
    /// session, the tab and the working tree are untouched (no restart, no
    /// `/clear`, no file written). The UI puts a confirmation in front of it for
    /// the same reason it does for [`Unlatch`](Self::Unlatch): if the judgement
    /// is wrong, a steered model gets its persistence channel back.
    ClearContamination,
    /// Step 4 — **restore.** The user rolled files back to a checkpoint. That
    /// cannot remove injected text from the model's context window, so this
    /// clears **nothing**: it arms
    /// [`TabLatch::awaiting_session_clear`], and the bit lifts only once cImp
    /// observes the tab start a new harness session.
    AwaitSessionClear,
}

impl LatchOverride {
    /// Parse a wire value. An unrecognized action is an **error**, never a
    /// benign default: the actions differ in exactly how much capability they
    /// hand back, so a typo must not pick one.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "flip_local" => Ok(LatchOverride::FlipLocal),
            "unlatch" => Ok(LatchOverride::Unlatch),
            "clear_contamination" => Ok(LatchOverride::ClearContamination),
            "await_session_clear" => Ok(LatchOverride::AwaitSessionClear),
            other => Err(format!(
                "invalid latch override `{other}` — expected one of \"flip_local\", \"unlatch\", \
                 \"clear_contamination\", \"await_session_clear\""
            )),
        }
    }

    /// The canonical wire value, and the `tool` column of the activity row.
    pub fn as_str(self) -> &'static str {
        match self {
            LatchOverride::FlipLocal => "flip_local",
            LatchOverride::Unlatch => "unlatch",
            LatchOverride::ClearContamination => "clear_contamination",
            LatchOverride::AwaitSessionClear => "await_session_clear",
        }
    }
}

/// The result of an applied override: what the latch was, and what the tab
/// looks like now. The prior state is carried because it is the fact the
/// activity row exists to record — "restored full access" means something very
/// different from `external` than from `local`.
#[derive(Debug)]
struct OverrideOutcome {
    prior: Latch,
    /// Step 4: the taint state before the move, for the same reason `prior`
    /// exists — "cleared the contamination flag" is only legible beside what was
    /// there. `Some` **only when this override actually released the bit**:
    /// `clear_contamination`, or an `unlatch` on a contaminated tab (decision
    /// 15's 2026-08-10 amendment). `None` for `flip_local`, for the arm, and for
    /// any move on an uncontaminated tab — which is what
    /// [`unlatch_clear_row`] keys its "write no row" decision on.
    prior_taint: Option<PriorTaint>,
    view: LatchView,
}

/// The result of a native-web beacon (#45): the tab's resulting view, which of
/// the two state changes it caused, and whether it is this tab-session's
/// reportable one (#48).
///
/// `report` is what makes the beacon's audit row bounded, and it is a stored
/// bit ([`TabLatch::beacon_flagged`]) rather than a derived one. A caller that
/// POSTs the route in a loop produces one row per tab-session, not one per
/// request; a feed a caller can flood is a feed that evicts the rows it exists
/// to keep.
///
/// #45 derived that bound from `engaged` alone, which silently dropped a whole
/// class of beacon — see [`contaminated_now`](Self::contaminated_now).
#[derive(Debug, PartialEq, Eq)]
struct BeaconOutcome {
    view: LatchView,
    /// The latch itself MOVED: Open → External. False when it was already
    /// External (sticky, and the fact is unchanged) **and** when the tab was
    /// latched `Local`, where the beacon cannot move it at all.
    engaged: bool,
    /// This beacon is what made the conversation contaminated — the bit went
    /// `false` → `true` here.
    ///
    /// #45 wrote a row only `if engaged`, so a beacon aimed at a `Local`-latched
    /// tab set `contaminated` unconditionally and recorded **nothing**: no row,
    /// no `warn!`, no `info!`. From that moment every `context_note` in the tab
    /// is quarantined and every external result enveloped, with the only
    /// evidence being the quarantine rows of the *later* writes. Locked decision
    /// 15 is unmoved — this records that the bit was SET, and nothing here or
    /// anywhere else clears it.
    contaminated_now: bool,
    /// Whether the handler should write this beacon's `injection_flag` row:
    /// something changed (`engaged || contaminated_now`) **and** this
    /// tab-session has not reported a beacon yet.
    report: bool,
}

impl BeaconOutcome {
    /// Nothing was touched: no policy in force, or no scope to engage.
    fn inert() -> Self {
        BeaconOutcome {
            view: LatchView::default(),
            engaged: false,
            contaminated_now: false,
            report: false,
        }
    }
}

/// Which tool-serving route a gate call is running for. The two differ in one
/// respect only: what an [`ToolClass::External`] classification *means* there.
///
/// `pub(super)` since #48 because the **worker** needs the same distinction and
/// was making the decision without it (review finding A-1). One definition of
/// the rule, in the module that first got it right, rather than a second copy
/// in `agent.rs` that can drift from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LatchRoute {
    /// A proxied `<server>__<tool>` id: `/mcp/call`, and the worker's
    /// MCP-host branch. Every name here is namespaced and therefore EXTERNAL
    /// by the Phase A unknown-⇒-EXTERNAL invariant; this route is the
    /// untrusted-content intake.
    Proxied,
    /// A cImp-native bare name: `/graph_run`'s `graph_*` / `context_*` tools,
    /// and the worker's native dispatch. This route physically cannot serve a
    /// proxied server's content, so a name that classifies EXTERNAL here is not
    /// external content at all: it is a typo or a hallucination that dispatch
    /// will reject as unknown. Letting it *engage* the latch would let one bad
    /// tool name poison a scope for its whole session — so on this route
    /// EXTERNAL neither latches nor is refused, and dispatch answers with its
    /// own error.
    Native,
    /// A **cImp-initiated hook**: the three `/context/*` shim routes (#48,
    /// finding M-7). Like [`LatchRoute::Native`] in what an EXTERNAL
    /// classification means — these routes serve fixed, cImp-owned names and
    /// physically cannot carry a proxied server's content — and different in
    /// exactly one respect, which is the whole reason the variant exists:
    /// **a hook may be REFUSED by a latch but must never MOVE one.**
    ///
    /// The calls arriving here are not tool calls. `PreToolUse`/`PreCompact`/
    /// `PostToolUse` fire automatically, for cImp's own automation, over work
    /// the harness has *already* permitted. Letting them engage would latch
    /// every tab with the read advisor or auto-check enabled to `Local` at its
    /// first read or edit, and every proxied web/MCP tool would be refused from
    /// that moment for a choice the model never made. The latch records what a
    /// CONVERSATION elected to do; cImp advising on a read the model had
    /// already been granted is not the conversation electing anything.
    ///
    /// It still *reads* the latch, because the other direction is the one M-7
    /// is about: under an EXTERNAL latch cImp must not execute the project's
    /// configured checks, or hand back repo source text, on behalf of a
    /// conversation that has ingested untrusted content.
    Hook,
}

impl LatchRoute {
    /// The route a tool name arrives on, by the one convention both dispatchers
    /// use: a namespaced `<server>__<tool>` id is proxied, a bare name is
    /// native (`agent.rs::HostRouter::call`, `mcp_host::call_for_consumer`).
    ///
    /// Never answers [`LatchRoute::Hook`]: that is a property of the ROUTE and
    /// not of the name, so the three hook handlers state it themselves.
    pub(super) fn of_tool(name: &str) -> Self {
        if name.contains("__") {
            LatchRoute::Proxied
        } else {
            LatchRoute::Native
        }
    }

    /// Whether an admitted call on this route may **move** the scope's latch.
    ///
    /// `false` only on [`LatchRoute::Hook`] — see that variant. A separate axis
    /// from [`external_is_content`](Self::external_is_content), deliberately: a
    /// hook has to be classified and gated (so it can be refused) without being
    /// elective (so it must not latch).
    pub(super) fn engages(self) -> bool {
        self != LatchRoute::Hook
    }

    /// Whether an [`ToolClass::External`] classification on this route really
    /// means **external content**.
    ///
    /// `false` on [`LatchRoute::Native`] and [`LatchRoute::Hook`] is the whole
    /// rule, and it is not a
    /// weakening of the unknown-⇒-EXTERNAL invariant: every proxied id contains
    /// `__` by construction, so the restrictive default still governs every
    /// name that can carry external content. What it excludes is the name that
    /// cannot — a misspelled `graph_symbols`, which is a hallucination, not a
    /// page.
    pub(super) fn external_is_content(self) -> bool {
        self == LatchRoute::Proxied
    }

    /// **Whether a gated call could actually EXECUTE on this route** — i.e.
    /// whether there is anything for the latch to be about. `false` means the
    /// gate must return without refusing and without moving the latch, and let
    /// the dispatcher answer with its own unknown-tool error.
    ///
    /// Two rules, one predicate, because they are the same principle applied to
    /// the two ways a name can fail to name a tool on a native route (#48,
    /// findings A-1 and M-2):
    ///
    /// 1. **Not in the table** — `class == External` on a route that cannot
    ///    carry a proxied server's content
    ///    ([`external_is_content`](Self::external_is_content)). A misspelled
    ///    `graph_symbols` is a hallucination, not a page.
    /// 2. **In the table but not dispatchable**
    ///    ([`toolclass::dispatchable`]). Six names are classified for reasons
    ///    other than being callable — the three `/context/*` hook routes' fixed
    ///    identities and Claude's own `Edit`/`Write`/`Bash`. Before this, a
    ///    model emitting the bare name `hook_post_edit` or `Bash` on
    ///    [`LatchRoute::Native`] classified LOCAL-CAPABILITY and latched the
    ///    scope to `Local` **before** dispatch rejected the name: the A-1 harm
    ///    (one bad tool name costs a scope the other half of its tools) in the
    ///    direction A-1's fix did not cover.
    ///
    /// Rule 2 is deliberately confined to [`LatchRoute::Native`], the only
    /// route whose name is model-supplied. [`LatchRoute::Hook`]'s name is
    /// composed by cImp and *is* the route's identity — applying rule 2 there
    /// would wave through the three hook routes M-7 exists to gate — and
    /// [`LatchRoute::Proxied`]'s names are the MCP host's to reject.
    ///
    /// This is not a weakening of unknown-⇒-EXTERNAL: it never admits a name
    /// into a *less* restrictive class, it only declines to record taint for a
    /// call that never runs. The containment question — may this class run at
    /// all — is still [`Latch::refusal`]'s, and every name that answers `true`
    /// here still faces it.
    pub(super) fn can_execute(self, name: &str, class: ToolClass) -> bool {
        if class == ToolClass::External && !self.external_is_content() {
            return false;
        }
        self != LatchRoute::Native || toolclass::dispatchable(name)
    }
}

/// What the calling ROUTE knows about a gated call that the registry cannot see
/// (#48, finding F-3): who asked for it, and — when the call is an intake —
/// where the content it is about to bring back is coming from.
///
/// Every field here is one the [`Screen::Contamination`](outbound::Screen)
/// row needs and the [`LatchRegistry`] has no way to derive. The registry owns
/// per-tab state; it does not see request bodies, does not parse tool
/// arguments, and cannot tell an IPC command from a loopback POST.
///
/// **Required at every call site, not defaulted.** The same rule
/// [`outbound::Flag::origin`] is under, for the same reason: #45 found that a
/// provenance column behind a defaulting constructor lets a new call site
/// inherit "cImp decided this" by writing nothing, which is the exact shape of
/// omission these rows exist to prevent. A native route states
/// [`CallProvenance::internal`] — with no URL, because a native route cannot
/// carry a fetched page — as a decision rather than by omission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallProvenance<'a> {
    /// Who asked for the state change a resulting row records.
    origin: outbound::Origin,
    /// The URL the call is fetching, when the route can see one. `/mcp/call`
    /// reads it out of the tool arguments (`detection::origin_of`) — the same
    /// pair its SSRF and detection rows carry, so a contamination row and the
    /// screen rows about the same call name the same page.
    url: Option<&'a str>,
    /// That URL's host — the at-a-glance column.
    host: Option<&'a str>,
    /// #48 F-16: the project this call runs against, in
    /// [`crate::activity::root_key`] form, for a row the registry writes when it
    /// has no scope to take one from ([`unattributed_write`]). `None` where the
    /// route genuinely has no project in view — `/latch/beacon` and the IPC
    /// override are about a tab, not a directory.
    ///
    /// Why the ROUTE and not the registry: [`LatchScope::root`] comes from the
    /// TAB (`tab_root_key`, F-3) precisely so a request body cannot redirect a
    /// contamination row. This field is the case where there IS no tab, and the
    /// only project in view is the one the call is about to write into — the same
    /// one the call's own `kind:"graph"` row files under, resolved by the same
    /// function (`GraphService::graph_root_key`), so the two rows for one call
    /// cannot name different projects.
    root: Option<&'a str>,
}

impl<'a> CallProvenance<'a> {
    /// cImp's own dispatch, executing a call it was already running, with no
    /// fetched content in view and no project in view either. Native routes that
    /// are about a TAB rather than a directory.
    const fn internal() -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: None,
        }
    }

    /// cImp's own dispatch on a native route that knows which project the call
    /// runs against (`/graph_run`). See [`Self::root`].
    const fn internal_in(root: &'a str) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url: None,
            host: None,
            root: Some(root),
        }
    }

    /// cImp's own dispatch over the proxied intake, naming the page it is
    /// about to read (either half may be absent — a search tool has arguments
    /// but no URL).
    ///
    /// No `root`: a PERSISTENT-WRITE cannot arrive on `/mcp/call` (every
    /// namespaced id classifies EXTERNAL), so the one row that reads
    /// [`Self::root`] is unreachable from here and a root passed in would be
    /// speculative.
    fn intake(url: Option<&'a str>, host: Option<&'a str>) -> Self {
        CallProvenance {
            origin: outbound::Origin::Internal,
            url,
            host,
            root: None,
        }
    }

    /// A loopback POST from a local process — the native-web beacon. Marked
    /// [`outbound::Origin::Http`] because the launch token is readable by
    /// anything running as this user, so a beacon is never evidence that the
    /// user acted (#45).
    const fn http() -> Self {
        CallProvenance {
            origin: outbound::Origin::Http,
            url: None,
            host: None,
            root: None,
        }
    }
}

/// The per-tab-session taint latches for the tools this proxy serves.
///
/// Locked decision 3: consumer enforcement lives here, keyed by V28 tab
/// identity. Two asymmetries with the worker's latch are deliberate:
///
/// - **Refusal, not def removal.** The worker rebuilds its advertised tool list
///   every turn, so decision 2's def removal is available to it. Consumers
///   cache `tools/list` at connect (the long-standing OpenCode behaviour that
///   forces a tab restart after MCP flag changes, and Claude does the same), so
///   removing a def mid-session would not be seen. The fixed-string refusals
///   from [`toolclass`] are the whole enforcement here.
/// - **Only the tools this proxy serves.** Claude's native Read/Bash and
///   OpenCode's bash/write never route through cImp, so no latch of ours can
///   reach them (decision 3's honest limit; OS containment is V33, optional
///   hook gating is Phase E).
#[derive(Default)]
struct LatchRegistry {
    tabs: Mutex<HashMap<(&'static str, String), TabLatch>>,
}

/// V32 Phase G (locked decision 16): the two feature switches one gated call
/// resolves, snapshotted by the handler that owns the settings read.
///
/// They are separate because they *are* separate features and can be switched
/// independently — and the interesting combination is the asymmetric one:
/// latch off + quarantine on still tracks contamination (so a note written
/// after a fetch is still held for review) while refusing nothing. The registry
/// takes them as data rather than reading settings itself, so the whole gate
/// stays a pure decision over one lock and one snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GatePolicy {
    /// [`Feature::TaintLatch`](crate::settings::injection::Feature::TaintLatch)
    /// — engagement, refusals and the latch shown in `/status`.
    latch: bool,
    /// [`Feature::MemoryQuarantine`](crate::settings::injection::Feature::MemoryQuarantine)
    /// — whether a PERSISTENT-WRITE from a contaminated conversation is stored
    /// held-for-review.
    quarantine: bool,
}

impl GatePolicy {
    /// Resolve both switches for one tab scope. `None` scope ⇒ the unknown
    /// caller's answer, the same fail-open reading `Scope::for_tab` takes
    /// (#48 F-35: `Scope::UnknownCaller` is what `Scope::App` was called at this
    /// site before locked decision 36; no behaviour moved).
    fn resolve(settings: &crate::settings::Settings, scope: Option<&LatchScope>) -> Self {
        use crate::settings::injection::{effective, Feature, Scope};
        let s = scope.map_or(Scope::UnknownCaller, LatchScope::injection);
        GatePolicy {
            latch: effective(Feature::TaintLatch, s, settings),
            quarantine: effective(Feature::MemoryQuarantine, s, settings),
        }
    }

    /// Neither control applies — nothing to decide, nothing to record.
    fn inert(self) -> bool {
        !self.latch && !self.quarantine
    }
}

/// V32 Phase G: read the app's live settings from managed state.
///
/// Every gated loopback handler already holds an `AppHandle`; this is the one
/// place that turns it into a `Settings`, so a handler cannot accidentally
/// resolve the hierarchy against a different snapshot than its neighbour. The
/// fallback is `Settings::default()` — all protection ON — because a request
/// arriving before managed state is up must not be the moment containment
/// silently lapses.
fn live_settings(app: &AppHandle) -> crate::settings::Settings {
    app.try_state::<crate::ipc::AppState>()
        .map(|s| s.settings.current())
        .unwrap_or_default()
}

/// One contamination TRANSITION, ready to record — see [`note_contamination`].
///
/// Owned rather than borrowed because the transition is detected under the
/// registry mutex and the row is written after it is dropped: `record_flag`
/// goes through `activity::record_bg`, whose contract is that it does file I/O
/// (inline off a tokio runtime), and holding a lock across that would put the
/// store's I/O on the critical path of every other tab's gated call.
struct Contamination {
    origin: outbound::Origin,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`], the same convention every other
    /// V32 row uses.
    scope: String,
    /// The conversation, when the registry entry knows it.
    session: Option<String>,
    tool: String,
    url: Option<String>,
    host: Option<String>,
    root: String,
    detail: String,
}

impl Contamination {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths: recording an event must not be able to fail the call it
    /// observes.
    fn record(self) {
        info!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            tool = %self.tool,
            host = self.host.as_deref().unwrap_or(""),
            root = %self.root,
            "loopback: V32 conversation became contaminated"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::Contamination,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // #48 F-29: derived, because this row's `scope` was built by
            // `LatchScope::label` (or, with no tab identity, by the `/mcp/call`
            // route's honest `"{agent}:(no tab identity)"`) — the two inputs
            // `scope_attribution` is defined over. The struct carries the label
            // rather than the scope, so there is no tab field to read here.
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: &self.tool,
            host: self.host.as_deref(),
            url: self.url.as_deref(),
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }
}

/// Step 4: on whose reasoning a contamination bit was released. The row's
/// `basis`, and the word that makes the audit trail legible — "the user said it
/// was a false positive" and "the user restored and then started a new session"
/// are very different claims about what is in the model's context window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClearBasis {
    /// [`LatchOverride::ClearContamination`] — the user judged the flagged
    /// content harmless. Nothing else changed.
    Resume,
    /// [`LatchOverride::AwaitSessionClear`] armed the tab after a restore, and
    /// cImp has now observed a new harness session.
    Restore,
    /// [`LatchOverride::Unlatch`] — decision 15's 2026-08-10 amendment. The user
    /// restored FULL access, and the flag went with it. A third basis rather
    /// than a reuse of [`Resume`](Self::Resume) because the two are different
    /// claims about what the user decided: `Resume` says *"that content was
    /// harmless"*, this says *"I am taking the whole risk knowingly"* — and an
    /// incident reviewer who cannot tell them apart cannot reconstruct the
    /// decision.
    Unlatch,
}

impl ClearBasis {
    /// The row's at-a-glance `tool` column. These rows have no tool call behind
    /// them; what happened is the fact worth reading.
    fn tool(self) -> &'static str {
        match self {
            ClearBasis::Resume => LatchOverride::ClearContamination.as_str(),
            ClearBasis::Restore => "session_clear_observed",
            // `"unlatch"` is also the `tool` column of the tab's
            // `latch_override` row. The two are told apart by `screen`, and
            // `contamination_events()` reads only the two contamination lanes,
            // so nothing joins them by accident — the shared word is a feature.
            ClearBasis::Unlatch => LatchOverride::Unlatch.as_str(),
        }
    }
}

/// One clearing of [`TabLatch::contaminated`], ready to record — the exact
/// counterpart of [`Contamination`], down to being owned rather than borrowed so
/// the row is written after the registry lock is dropped.
///
/// **Both authorised paths build one**, which is what stops the two from
/// describing the same state change differently: the immediate resume in
/// [`LatchRegistry::apply_override`], and the armed rotation firing inside
/// [`TabLatch::observe`] (via [`ClearedOnRotation::into_row`]).
struct ContaminationCleared {
    /// Who acted *now*. [`outbound::Origin::Ipc`] for the resume — a human in
    /// the app's own UI. [`outbound::Origin::Internal`] for the armed rotation:
    /// the authority was the earlier click (which has its own
    /// [`outbound::Screen::LatchOverride`] row), but the act recorded here is
    /// cImp's own observation, and `Ipc` means "a human did *this*".
    origin: outbound::Origin,
    basis: ClearBasis,
    consumer: &'static str,
    /// `agent:tab` — [`LatchScope::label`].
    scope: String,
    /// The conversation the row is filed under: the one contamination was
    /// cleared *for*. For the rotation path that is the OUTGOING session, not
    /// the new one — the new one was never contaminated, and filing it there
    /// would break a join against the `contamination` row that opened the
    /// lifecycle.
    session: Option<String>,
    root: String,
    detail: String,
}

impl ContaminationCleared {
    /// Write the row. Fire-and-forget, like every other `record_flag` call on
    /// these paths.
    fn record(self) {
        warn!(
            target: "offload",
            consumer = self.consumer,
            scope = %self.scope,
            basis = self.basis.tool(),
            origin = self.origin.as_str(),
            root = %self.root,
            "loopback: V32 contamination flag cleared on the user's authority"
        );
        outbound::record_flag(outbound::Flag {
            screen: outbound::Screen::ContaminationCleared,
            origin: self.origin,
            consumer: self.consumer,
            scope: &self.scope,
            // Derived from the label this row was built with — see the
            // contamination row above (#48 F-29).
            attribution: outbound::scope_attribution(&self.scope),
            session: self.session.as_deref(),
            tool: self.basis.tool(),
            host: None,
            url: None,
            resolved_ip: None,
            canary: false,
            root: self.root.clone(),
            detail: &self.detail,
        });
    }

    /// Record a clear that may not have happened. Every `observe` call site
    /// funnels through this, so "the arm fired here" needs no branch of its own
    /// at five sites.
    fn record_from(cleared: Option<ClearedOnRotation>, scope: &LatchScope) {
        if let Some(ev) = cleared {
            ev.into_row(scope).record();
        }
    }
}

/// The sentence an incident reviewer reads when the bit was released —
/// composed **once**, for both paths.
///
/// Written as one function rather than two format strings because the whole
/// point of the pair is that they say the same things about different bases: the
/// prior state, what was and was not restored by the clear, and — the part a
/// reviewer most needs — that quarantined notes are untouched by it.
fn clear_detail(
    basis: ClearBasis,
    origin: outbound::Origin,
    prior_latch: &str,
    prior_session: Option<&str>,
    new_session: Option<&str>,
) -> String {
    let how = match basis {
        ClearBasis::Resume => "the user judged the flagged content harmless and cleared the flag \
                               from the taint popover. The session, the tab and the working tree \
                               were not touched"
            .to_string(),
        ClearBasis::Unlatch => "the user restored FULL access from the taint popover (`unlatch`), \
                                and decision 15's 2026-08-10 amendment releases the flag with it: \
                                that click already hands back the strictly more dangerous \
                                capability — read AND web with the injected content still in the \
                                context window — so quarantining persistent memory afterwards \
                                would overrule the judgement it just asked for. An attacker \
                                cannot click it; the trust root is authority, not evidence. The \
                                session, the tab and the working tree were not touched"
            .to_string(),
        ClearBasis::Restore => format!(
            "the user restored a checkpoint, which armed a ONE-SHOT clear (a restore rolls back \
             files and cannot remove injected text from a context window, so the flag was kept), \
             and cImp has now observed this tab start a new harness session{}. The arm is the \
             authority here; the rotation only answers \"has it happened yet\", which is why a \
             forgeable rotation signal is acceptable for it and for nothing else",
            match new_session {
                Some(s) => format!(" ({s})"),
                None => String::new(),
            }
        ),
    };
    // The one sentence the three bases cannot share: two of them leave the latch
    // exactly where it was, and the third IS a latch move.
    let latch_note = match basis {
        ClearBasis::Unlatch => "The same click also moved the latch to `open` — that is what \
                                released the flag, and it is recorded as its own `latch_override` \
                                row.",
        ClearBasis::Resume | ClearBasis::Restore => {
            "The latch itself is unchanged by this and keeps its own controls."
        }
    };
    format!(
        "CONTAMINATION CLEARED (basis: {}, origin: {}): {how}. Prior state: contaminated=true, \
         latch={prior_latch}, session={}. {latch_note} Memory notes already quarantined STAY \
         quarantined — promoting or discarding them is the Memory view's own review (locked \
         decision 10), a separate consent surface. What changes is that this tab's future \
         persistent writes are stored clean again, and that a fresh contamination will report \
         itself as a new transition.",
        basis.tool(),
        origin.as_str(),
        prior_session.unwrap_or("unknown"),
    )
}

impl ClearedOnRotation {
    /// Turn a lock-side clear into the row its caller's scope can file.
    fn into_row(self, scope: &LatchScope) -> ContaminationCleared {
        debug_assert!(self.armed, "only an armed tab can clear on a rotation");
        ContaminationCleared {
            // NOT `Ipc`: see the field's doc. The click that authorised this
            // happened earlier and was recorded then.
            origin: outbound::Origin::Internal,
            basis: ClearBasis::Restore,
            consumer: scope.agent,
            scope: scope.label(),
            session: Some(self.prior_session.clone()),
            root: scope.root.clone(),
            detail: clear_detail(
                ClearBasis::Restore,
                outbound::Origin::Internal,
                self.prior_latch,
                Some(&self.prior_session),
                Some(&self.session),
            ),
        }
    }
}

/// **The one place a conversation is marked contaminated** (#48, finding F-3).
///
/// Both paths that can set the bit — an admitted proxied EXTERNAL call in
/// [`LatchRegistry::gate`], and the native-web beacon in
/// [`LatchRegistry::beacon`] — flip it *here*, so the transition cannot be set
/// on one path and recorded on the other, and a third path added later gets the
/// row by calling the only function that sets the bit.
///
/// # What it records, and what it deliberately does not
///
/// It records the **transition**, false → true, not every contaminating call.
/// Later EXTERNAL calls restate a fact this row already carries, and each
/// already writes an ordinary proxied-MCP activity row of its own; what had no
/// record at all was the moment the conversation stopped being clean. The
/// `mem::replace` below *is* the claim, so this is self-limiting and needs no
/// separate claim bit of the [`TabLatch::latch_flagged`] kind.
///
/// Because the bit is sticky across session rotations (H-2 — see
/// [`TabLatch::contaminated`]), "once" here means **once per tab**, not once
/// per conversation: a `/clear` in a contaminated tab keeps the taint and
/// writes no second row, and the row's `session` therefore names the
/// conversation contamination started in. A consumer joining these rows to
/// conversation-scoped state has to read them that way.
///
/// It does **not** decide whether the call contaminates. That is the caller's
/// classification (`gate` calls it only for `ToolClass::External` on a route
/// where EXTERNAL means content; the beacon calls it unconditionally, because a
/// beacon *is* the harness reporting that it read a page). Nothing about when
/// contamination is SET changes here — this is observability over an unchanged
/// decision.
///
/// # Why the return value must be used
///
/// The detection happens under the registry mutex and the write happens after
/// it is released, so the two are necessarily separate statements. `#[must_use]`
/// is what keeps them from drifting apart: a path that flips the bit and drops
/// the result fails the build under `-D warnings`, which is the same
/// "compile-time or it will be forgotten" posture `declare_screens!` takes for
/// the retention lane.
#[must_use = "a contamination transition that is detected and not recorded is finding F-3 again — \
              call Contamination::record() after dropping the registry lock"]
fn note_contamination(
    entry: &mut TabLatch,
    scope: &LatchScope,
    tool: &str,
    prov: CallProvenance<'_>,
) -> Option<Contamination> {
    if std::mem::replace(&mut entry.contaminated, true) {
        return None;
    }
    Some(Contamination {
        origin: prov.origin,
        consumer: scope.agent,
        scope: scope.label(),
        // The registry entry's session, not the scope's: `observe` has already
        // run by the time any caller reaches here, so this is the session the
        // latch itself considers current — the one a later join has to match.
        session: entry.session.clone(),
        tool: tool.to_string(),
        url: prov.url.map(str::to_string),
        host: prov.host.map(str::to_string),
        root: scope.root.clone(),
        detail: format!(
            "CONTAMINATED: external content entered this conversation via {tool}{}. Nothing was \
             refused — the call was admitted, and this row records the state change it caused. \
             From here on every persistent memory write from this tab is quarantined for review \
             and every external result keeps its spotlighting envelope (latch={}). No \
             filesystem-derived signal clears the bit (H-2: a new harness session is not proof of \
             a new context window, because the model's own shell can forge one) and no HTTP route \
             can. It is cleared only by the USER, from the taint popover — immediately \
             (`clear_contamination`, \"that content was harmless\"), by restoring FULL access \
             (`unlatch`, which accepts the larger risk deliberately), or after a checkpoint \
             restore (`await_session_clear`, effective once cImp observes a new harness session). \
             The workflow flip (`flip_local`) does NOT clear it. Whichever happens, it writes its \
             own `contamination_cleared` row.",
            match prov.host {
                Some(h) => format!(" from {h}"),
                None => String::new(),
            },
            entry.latch.label(),
        ),
    })
}

/// #48 (2026-08-08 re-review), finding M-19 — what [`LatchRegistry::gate`]
/// hands back to a caller it could not attribute to a tab.
///
/// # The asymmetry this closes
///
/// The identity-less fail-open is locked (F-5/H-8) and load-bearing: a child
/// spawned before `--tab` existed, and the documented headless consumers, must
/// keep their TOOL-SERVING routes. Nothing here touches that — every class
/// except one still leaves this function `Clean`, and no latch row is created
/// for any of them.
///
/// PERSISTENT-WRITE is the exception, and the precedent for treating it as one
/// is already in this codebase, one module over: on the **headless** path a
/// write with no identity is refused outright (`graph::mcp::headless_refusal`,
/// `HEADLESS_WRITE_UNAVAILABLE`), on exactly this reasoning — that path has
/// neither a session identity nor a taint verdict, and a note written blind
/// with neither is *"project-wide, permanent, unattributable AND unquarantined,
/// which is the highest-privilege write the memory surface offers"*. The
/// loopback path reached the identical state and stored the note clean. Two
/// paths, the same two missing facts, opposite answers.
///
/// # Why quarantine and not the headless path's refusal
///
/// Locked decision 10. A refusal on this path throws away the legitimate
/// research conclusion the session existed to produce; the quarantine keeps it,
/// flags it, hides it from `context_recall` / `context_notes` / compaction
/// carry-over / the fact distiller, and hands the user promote-or-discard. (The
/// headless path refuses because there is no running app to review a queue in,
/// not because refusal is the better answer.)
///
/// It is [`WriteTaint::Unattributed`] rather than `Quarantined` so the model is
/// told the actual reason — see that variant.
///
/// # Deliberately not gated on `policy.latch`
///
/// Only on `policy.quarantine`, matching the scoped path: locked decision 16
/// keeps the two switches independent, and this is a quarantine decision, not a
/// latch decision. Nothing here reads or moves a latch — there is no scope to
/// move one for, which is the whole point.
///
/// #48 F-16: `prov` is here for one field — [`CallProvenance::root`]. The
/// finding's own wording, *"`LatchRegistry::gate` has no scope to derive a root
/// from"*, is true of `gate` and **false of the route that calls it**: the only
/// route that can reach this line with a PERSISTENT-WRITE is `/graph_run`, and
/// that handler holds the project the note is about to be written into.
fn unattributed_write(
    policy: GatePolicy,
    route: LatchRoute,
    name: &str,
    prov: CallProvenance<'_>,
) -> WriteTaint {
    let class = toolclass::classify(name);
    if !policy.quarantine || class != ToolClass::PersistentWrite || !route.can_execute(name, class)
    {
        return WriteTaint::Clean;
    }
    warn!(
        target: "offload",
        tool = %name,
        "loopback: persistent memory write held — the caller carries no resolvable tab identity"
    );
    // One row per held note, for the same reason the scoped quarantine writes
    // one: each is a separate item in the user's review queue, and a feed that
    // reported only the first would leave later ones discoverable solely by
    // opening the Memory view. There is no scope, so the columns that name one
    // say so rather than guessing — the `consumer` a request body could have
    // supplied is exactly the field M-19 showed is caller-chosen.
    outbound::record_flag(outbound::Flag {
        screen: outbound::Screen::MemoryQuarantine,
        origin: outbound::Origin::Internal,
        consumer: "unattributed",
        scope: "unattributed",
        // #48 F-29 — the reason this field exists. `"unattributed"` is a
        // description, not a scope, and the old derivation (anything without a
        // `:` ⇒ `Headless`) turned it into the positive claim *"a run with no tab
        // behind it"*. This frame does not know that: it knows only that the
        // caller's tab identity did not resolve, which is what
        // `Attribution::Unattributed` says and what this whole row is about.
        //
        // Not more precise than that ON PURPOSE. The route's `LatchScoping`
        // could distinguish "no id was sent" (`Headless`) from "an id that names
        // no configured tab" (`Unrecognized`), but `gate` receives
        // `Option<&LatchScope>` and both collapse to `None` before this line —
        // the same collapse F-20 fixed one seam over. Recovering it means
        // threading the attribution through `CallProvenance`, a separate change;
        // claiming either half from here would be a guess.
        attribution: crate::activity::Attribution::Unattributed,
        session: None,
        tool: name,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        // #48 F-16: the route's project, not an empty string. There is no scope
        // to take a root from — that is this function's whole premise — but the
        // ROUTE knows which project the note is about to be written into, and
        // that is the project a reviewer filters by. `None` would be a route with
        // no project in view, which cannot reach this line today; the fallback is
        // still empty rather than invented, and `""` is a positive claim of
        // ignorance with a documented meaning (see `ActivityEntry::root`).
        root: prov.root.unwrap_or_default().to_string(),
        detail: toolclass::UNATTRIBUTED_WRITE_NOTICE,
    });
    WriteTaint::Unattributed
}

/// #48 (F-34) — pick the refusal that states the cause the gate **checked**,
/// for the one latch position that has two possible causes.
///
/// # What this is, and what it is deliberately not
///
/// It is a **message selector**, not a gate. Containment is decided entirely by
/// [`Latch::proxy_gate`](toolclass::Latch::proxy_gate) before this runs and is
/// byte-identical to what it always was: `Some(_)` in, `Some(_)` out, and the
/// same calls refused. `local_by_user_flip` never joins the guard — for F-13's
/// reason, an unknown value must be able to cost only the better *message* and
/// never the refusal, so a `false` here simply serves the pre-F-34 constant.
///
/// # Why it is here and not in [`Latch::refusal`](toolclass::Latch::refusal)
///
/// **Locked decision 34 places the choice in `LatchRegistry::gate`, which holds
/// `TabLatch::local_by_user_flip`, and NEVER in `Latch::refusal`** — and that is
/// load-bearing rather than stylistic. `Latch::refusal` is a *pure function over
/// [`Latch`](toolclass::Latch)* that the **offload worker** also calls
/// (`offload::agent`), and the worker has no user-flip concept to thread:
/// migrating this down would either break it or force it to pass a meaningless
/// `false` forever. The rule is a convention, not a type, so it is also guarded
/// by a tripwire in `toolclass`
/// (`the_user_flip_constant_is_never_reachable_from_the_pure_latch_functions`).
///
/// Written as a free function taking the bool rather than a `TabLatch` method so
/// the ONE fact it may consult is visible in its signature: nothing from the
/// caller, the model or the tool arguments can reach the string.
///
/// # The match, and why on the constant
///
/// `REFUSAL_EXTERNAL_BLOCKED` is produced by exactly one state — `Latch::Local`
/// blocking [`ToolClass::External`] — so keying on it is equivalent to keying on
/// that pair, and it cannot silently capture a future refusal that means
/// something else. The other two constants are unreachable under a `Local`
/// latch, so they fall through untouched.
fn user_flip_refusal(refusal: &'static str, local_by_user_flip: bool) -> &'static str {
    if local_by_user_flip && refusal == toolclass::REFUSAL_EXTERNAL_BLOCKED {
        // The user's own IPC flip closed the external side. Saying a tool call
        // did it is F-23's defect on the route that ships ON.
        toolclass::REFUSAL_EXTERNAL_USER_LOCAL
    } else {
        refusal
    }
}

impl LatchRegistry {
    /// Decide one call, and engage the latch when it may proceed.
    ///
    /// The whole check-then-engage runs under one lock and **before** the call
    /// executes — loopback serves concurrent requests, so two simultaneous
    /// calls from one tab must not both observe an open latch. A refused call
    /// never engages or flips anything (same property as Phase A's
    /// `latch_gate`): otherwise a hallucinated call to the blocked side could
    /// redefine which side of the boundary the session is on.
    ///
    /// V32 Phase C2: the success arm now carries a [`WriteTaint`]. It is
    /// [`WriteTaint::Quarantined`] for exactly one case — a PERSISTENT-WRITE
    /// under an EXTERNAL latch, which Phase B refused and locked decision 10
    /// turns into a quarantined write — and `Clean` for everything else. The
    /// caller must thread it into the call it is about to make; ignoring it
    /// would store an externally-influenced note as ordinary memory.
    ///
    /// V32 Phase G: `policy` carries the two feature switches (locked decision
    /// 16). With both off the gate returns immediately without touching any
    /// state — a disabled control must leave no trace, not merely no verdict,
    /// or `/status` would keep showing latches the user turned off.
    ///
    /// #48 (F-3): `prov` is what the calling route knows and the registry
    /// cannot derive — see [`CallProvenance`]. It is used for exactly one thing
    /// here: the [`Screen::Contamination`](outbound::Screen) row, written when
    /// an admitted call is the one that stops this conversation being clean.
    fn gate(
        &self,
        scope: Option<&LatchScope>,
        route: LatchRoute,
        name: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> Result<WriteTaint, &'static str> {
        if policy.inert() {
            return Ok(WriteTaint::Clean);
        }
        // Fail-open: no tab identity ⇒ no latch (see [`latch_scope`]) — except
        // for the one class where "we do not know who this is" is itself the
        // hazard. See [`unattributed_write`].
        let Some(scope) = scope else {
            return Ok(unattributed_write(policy, route, name, prov));
        };
        let class = toolclass::classify(name);
        // #48, findings A-1 and M-2: a call that cannot execute on this route
        // is not evidence of anything, so it neither latches nor is refused —
        // see [`LatchRoute::can_execute`] for both rules and why this does not
        // weaken unknown-⇒-EXTERNAL.
        if !route.can_execute(name, class) {
            return Ok(WriteTaint::Clean);
        }
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        // Step 4: for a Claude tab this is the usual place an armed one-shot
        // fires — the first cImp tool call of the conversation that followed the
        // user's `/clear`. Recorded on every exit below, before this call's own
        // rows, so the feed reads in the order the state actually moved.
        let rotated = entry.observe(scope.session.as_deref());
        // V32 Phase F: quarantine keys on CONTAMINATION, not on the latch
        // position. `Latch::External` always implies `contaminated` (both are
        // set by the same admitted call, a few lines below), so this only ever
        // WIDENS the pure-latch verdict `proxy_gate` computes — to the one case
        // decision 15 creates, where a user override moved the latch off
        // External on a conversation that has already read external content.
        // The pure function stays the single definition of the latch's own
        // semantics; the bit is layered over it here, at the only site that
        // owns per-conversation state.
        //
        // V32 Phase G layers the two switches over the same expression, and the
        // order is the whole point: the latch's verdict is computed only when
        // the latch feature is on, and the quarantine verdict only when the
        // quarantine feature is on. So "latch off, quarantine on" still holds a
        // note written after a fetch (contamination is tracked below regardless
        // of the latch), and "latch on, quarantine off" refuses the same calls
        // it always did while storing writes clean.
        let latched = if policy.latch {
            entry.latch.proxy_gate(class)
        } else {
            ProxyGate::Proceed(WriteTaint::Clean)
        };
        let decision = match latched {
            ProxyGate::Proceed(WriteTaint::Clean)
                if policy.quarantine
                    && class == ToolClass::PersistentWrite
                    && entry.contaminated =>
            {
                ProxyGate::Proceed(WriteTaint::Quarantined)
            }
            ProxyGate::Proceed(WriteTaint::Quarantined | WriteTaint::Unattributed)
                if !policy.quarantine =>
            {
                ProxyGate::Proceed(WriteTaint::Clean)
            }
            other => other,
        };
        let refusal = match decision {
            // `Unattributed` cannot arrive here — it is [`unattributed_write`]'s
            // answer, returned above this frame's `scope` binding, and
            // `proxy_gate` never produces it. It is bound rather than excluded
            // so that a future path which does route one through holds the note
            // (and explains it correctly) instead of falling into the `Clean`
            // arm below.
            ProxyGate::Proceed(held @ (WriteTaint::Quarantined | WriteTaint::Unattributed)) => {
                // Locked decision 10: store it, flag it, hold it for the user.
                // The write itself never latches (PERSISTENT-WRITE is not a
                // latching class), so nothing about the scope changes here.
                warn!(
                    target: "offload",
                    agent = scope.agent,
                    tab = %scope.tab,
                    tool = %name,
                    latch = entry.latch.label(),
                    "loopback: persistent memory write quarantined by the V32 session taint latch"
                );
                // Unlike the refusal below this is NOT one-row-per-scope: each
                // quarantined note is a separate item in the user's review
                // queue, and a feed that reported only the first would leave
                // later ones discoverable solely by opening the Memory view.
                drop(tabs);
                ContaminationCleared::record_from(rotated, scope);
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::MemoryQuarantine,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // #48 F-29: the scope is in hand, so the tab id comes from
                    // its field rather than from a re-split label.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: held
                        .write_notice()
                        .unwrap_or(toolclass::QUARANTINE_WRITE_NOTICE),
                });
                return Ok(held);
            }
            ProxyGate::Proceed(WriteTaint::Clean) => None,
            // #48 (F-34): the message, and ONLY the message. `decision` above is
            // untouched, so what gets refused is byte-identical to what always
            // did — this arm just picks which fixed constant states the cause,
            // from a fact only this frame holds. See [`user_flip_refusal`].
            ProxyGate::Refuse(r) => Some(user_flip_refusal(r, entry.local_by_user_flip)),
        };
        if let Some(refusal) = refusal {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch = entry.latch.label(),
                "loopback: tool call refused by the V32 session taint latch"
            );
            // V32 Phase C: Phase B left this refusal without a consumer — the
            // user could only see it as a tool that mysteriously stopped
            // working. One row per scope (see `TabLatch::latch_flagged`).
            let first = !std::mem::replace(&mut entry.latch_flagged, true);
            drop(tabs);
            ContaminationCleared::record_from(rotated, scope);
            if first {
                outbound::record_flag(outbound::Flag {
                    screen: outbound::Screen::LatchRefusal,
                    origin: outbound::Origin::Internal,
                    consumer: scope.agent,
                    scope: &scope.label(),
                    // The scope is in hand — see `LatchScope::attribution`.
                    attribution: scope.attribution(),
                    session: scope.session.as_deref(),
                    tool: name,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: scope.root.clone(),
                    detail: refusal,
                });
            }
            return Err(refusal);
        }
        // V32 Phase F: the call is admitted, so if it is EXTERNAL its content is
        // about to enter this conversation. Set the contamination bit HERE
        // rather than deriving it from the latch, because the latch is now
        // user-movable and the bit is not. (A refused call never reaches this
        // point, so a hallucinated call to the blocked side cannot contaminate
        // a clean session — the same property `engage` has.)
        //
        // V32 Phase G: tracked whenever EITHER switch is on (an inert policy
        // returned above), because contamination is the quarantine's input as
        // much as the latch's — a user who keeps quarantine but drops the latch
        // still needs "this conversation read a page" to be true.
        //
        // #48 (F-3): and the TRANSITION is recorded, through the one function
        // that owns the bit. This was the finding's whole substance — the line
        // this replaces set the bit silently, and the only trace was the `info!`
        // below, which fires on the *latch* transition. A tab already latched
        // `Local`, or one running with the latch feature off and the quarantine
        // on, contaminated with no timestamp, no tool and no row. The condition
        // is unchanged, deliberately: recording must follow the same rule the
        // bit does, or the switch combination that made it silent still would.
        //
        // Engagement is the LATCH's own state, so it moves only while the latch
        // feature is on: a latch shown as engaged in `/status` while the feature
        // is off would describe a boundary that is not being enforced. It is
        // sequenced ahead of the contamination note for one reporting reason —
        // the row quotes the latch this call leaves the tab in, so a fresh tab's
        // contamination row must say `external` rather than the `open` it was a
        // microsecond earlier. Nothing can observe the entry between the two
        // (both run under the one lock), so the order is a choice about the row,
        // not about the semantics.
        //
        // #48 (M-7): …and only on a route whose calls are ELECTIVE. A
        // [`LatchRoute::Hook`] call is cImp's own automation firing over work
        // the harness already permitted, so it reads the latch (it can be
        // refused, three lines up) and never moves it. `engages()` is checked
        // here rather than inside `Latch::engage` because it is a fact about
        // the route, not about the class.
        let engaged = policy.latch && route.engages() && entry.latch.engage(class);
        let contamination = if class == ToolClass::External {
            note_contamination(entry, scope, name, prov)
        } else {
            None
        };
        let latch = entry.latch.label();
        // Both the log line and the row are written with the lock released —
        // `record_flag` reaches the activity store, which does file I/O.
        // (Step 4's clear row is written first, below, for the same ordering
        // reason the beacon states.)
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        if engaged {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %name,
                latch,
                "loopback: V32 session taint latch engaged"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        Ok(WriteTaint::Clean)
    }

    /// V32 Phase F (locked decision 14): engage this tab's EXTERNAL latch on
    /// behalf of a HARNESS-NATIVE web tool that never routed through cImp.
    ///
    /// The beacon is the sensor mode's whole mechanism: Claude's `WebFetch` /
    /// `WebSearch` and OpenCode's `webfetch` / `websearch` bypass the proxy, so
    /// without this a tab could read an attacker's page while `/status` still
    /// says `open` and every proxied local-capability tool stays available
    /// beside it. It does exactly what an admitted proxied EXTERNAL call does —
    /// engage the latch, set the contamination bit — and deliberately nothing
    /// more:
    ///
    /// - **No refusal, ever.** The tool has already been permitted by the
    ///   harness by the time the hook runs (and in `deny` mode it never runs at
    ///   all). Returning "blocked" here would be a lie the caller cannot act on.
    /// - **Fail-open on identity**, like every other gate here: a beacon with no
    ///   tab id — or, since #45, with an id that is not a configured tab — has
    ///   no scope to engage.
    ///
    /// It reports what changed ([`BeaconOutcome`]) rather than writing the row
    /// itself: the row's honesty depends on the [`outbound::Origin`] of the
    /// request that caused it, and the registry cannot see that. The handler
    /// owns it (#45).
    ///
    /// The one asymmetry with `gate`: a beacon arriving while the tab is
    /// LOCAL-latched cannot refuse the fetch (it already happened), so it
    /// records the contamination and leaves the latch where it is — the honest
    /// reading of "this conversation has now seen external content, and its
    /// proxied external side stays closed". That case is exactly the one #45
    /// left unaudited; see [`BeaconOutcome::contaminated_now`] (#48).
    ///
    /// V32 Phase G: gated by the same [`GatePolicy`] a proxied call resolves.
    /// An inert policy answers with the default view and records nothing — a
    /// beacon whose latch and quarantine are both off has nothing to report to.
    ///
    /// #48 (F-3): the [`Screen::LatchBeacon`](outbound::Screen) row the handler
    /// writes is unchanged and still says what it always said — *a native web
    /// tool was detected*. The contamination row this method now writes says
    /// something different — *this conversation stopped being clean* — and a
    /// beacon into an already-contaminated tab writes only the first. `prov`
    /// carries the [`outbound::Origin`] for the same reason the handler states
    /// it for the beacon row: over this route it is `Http`, and that is a fact
    /// about the caller, not about the tab.
    fn beacon(
        &self,
        scope: Option<&LatchScope>,
        tool: &str,
        policy: GatePolicy,
        prov: CallProvenance<'_>,
    ) -> BeaconOutcome {
        if policy.inert() {
            return BeaconOutcome::inert();
        }
        let Some(scope) = scope else {
            return BeaconOutcome::inert();
        };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = tabs.entry(scope.key()).or_insert_with(TabLatch::fresh);
        let cleared = entry.observe(scope.session.as_deref());
        // Unchanged, deliberately: contamination is set on every beacon, and
        // nothing on THIS route can ever clear it (locked decision 15 — step 4's
        // two clears are user actions over IPC, and `observe` above releases the
        // bit only for a tab the user armed). What #48
        // adds is only that the TRANSITION is observable, so it can be recorded
        // — through the SAME function the proxied gate flips the bit with, so
        // the two paths cannot disagree about what a transition is or produce
        // two shapes of row for it. Ordered after the engagement for the same
        // reporting reason `gate` states: the row quotes the latch this beacon
        // leaves the tab in.
        let moved = policy.latch && entry.latch.engage(ToolClass::External);
        let contamination = note_contamination(entry, scope, tool, prov);
        let contaminated_now = contamination.is_some();
        // One row per tab-session over BOTH transitions, rather than one per
        // transition kind: a policy change mid-session could otherwise produce a
        // second row for the same conversation.
        let report =
            (moved || contaminated_now) && !std::mem::replace(&mut entry.beacon_flagged, true);
        let view = entry.view();
        drop(tabs);
        // Step 4: recorded BEFORE the contamination row this beacon may also
        // produce. A beacon arriving on the first call after an armed rotation
        // clears the bit and immediately re-sets it, and the feed has to read in
        // that order to make sense.
        ContaminationCleared::record_from(cleared, scope);
        if moved {
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 session taint latch engaged by a native-web beacon"
            );
        } else if contaminated_now {
            // The case #45 left entirely silent: the latch did not move (the tab
            // is latched `Local`, or the latch feature is off) but this
            // conversation is contaminated from here on.
            info!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                latch = view.latch,
                "loopback: V32 conversation marked contaminated by a native-web beacon (latch unmoved)"
            );
        }
        if let Some(contamination) = contamination {
            contamination.record();
        }
        BeaconOutcome {
            view,
            engaged: moved,
            contaminated_now,
            report,
        }
    }

    /// V32 Phase H: this tab's current view, **read-only** — the state the
    /// OpenCode plugin's native-tool gate decides against.
    ///
    /// Two properties, both deliberate:
    ///
    /// - **It does not create an entry.** A tab that has never made a gated call
    ///   has nothing to report, and materializing a row for every poll would put
    ///   tabs in `/status` that no tool call ever touched. Absent ⇒
    ///   [`LatchView::default`] ⇒ `open` ⇒ the gate denies nothing. Fail-open by
    ///   construction, not by a branch someone has to remember.
    /// - **It DOES `observe`.** A stale `external` left over from a rotated
    ///   session would deny `read`/`bash` for a whole fresh conversation — a
    ///   false deny of the harness's core tools, which is far worse than the
    ///   read-only purity of not touching the entry. `observe` is the same
    ///   rotation rule `gate` and `beacon` apply, so the three cannot disagree
    ///   about when a conversation ended.
    ///
    /// Step 4: which also means this is one of the places an armed one-shot
    /// fires. For an OpenCode tab it is the *usual* one — the plugin polls
    /// `/latch/state` around the harness's own turns, so a `/clear` after a
    /// restore lifts the bit without waiting for a proxied tool call.
    fn view_for(&self, scope: &LatchScope) -> LatchView {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let (view, cleared) = match tabs.get_mut(&scope.key()) {
            Some(entry) => {
                let cleared = entry.observe(scope.session.as_deref());
                (entry.view(), cleared)
            }
            None => (LatchView::default(), None),
        };
        drop(tabs);
        ContaminationCleared::record_from(cleared, scope);
        view
    }

    /// V32 Phase F (locked decision 15): apply a USER-initiated latch move.
    ///
    /// Decision 1 rejected automatic resets and still does — an injected context
    /// stays injected, so the latch never *releases itself*. What decision 15
    /// adds is a human, who knows something the system cannot infer: that the
    /// research is done and its output has been read.
    ///
    /// **What enforces "a human" (#45).** This used to claim "nothing the model
    /// can reach may move this", which was false: the same implementation was
    /// also exposed as `POST /latch/override`, behind nothing but the per-launch
    /// bearer token — and that token is readable by any process running as the
    /// user (`.cimp-offload.json`, `.cimp-discovery/<pid>.json`, and the
    /// generated OpenCode plugin inside the project tree). That route is GONE.
    /// What the code now enforces, exactly:
    ///
    /// - **The only caller is the capability-scoped `latch_override` Tauri IPC
    ///   command**, driven by the badge popover. The webview holds no bearer
    ///   token and makes no HTTP call, so this path is not reachable from
    ///   outside the app process.
    /// - **What a shell-capable model CAN still reach is `/latch/beacon`**,
    ///   which only ever tightens (Open → External) and only for a configured
    ///   tab id ([`is_configured_tab`]) — it cannot flip to Local, cannot
    ///   unlatch, and cannot clear contamination.
    /// - **What clears `contaminated`** is three of the four actions —
    ///   `clear_contamination`, `unlatch` (decision 15's 2026-08-10 amendment)
    ///   and, deferred, `await_session_clear` — and nothing else: no automatic
    ///   path, no HTTP path. `flip_local` is the one that does not. See
    ///   [`TabLatch::contaminated`] for why a click is a legitimate trust root
    ///   where a transcript file is not.
    ///
    /// This is not an integrity boundary against native code, and never was —
    /// decision 3 says plainly that a model with a shell already has the
    /// capabilities the latch withholds. It is the difference between an audit
    /// trail that records a user's decision and one that records a POST.
    ///
    /// **The feature switches are deliberately not consulted.** `gate` creates
    /// no entry while [`GatePolicy::inert`], so with both controls off there is
    /// usually nothing here to move and the caller gets the "nothing to
    /// override" error. But an entry created while the controls were ON survives
    /// the user switching them off, and its contamination bit is still what the
    /// badge renders — so refusing to clear it would leave a stale flag the user
    /// cannot reach *because* they disabled the feature. Every action here is
    /// user-initiated and only ever loosens cImp's own bookkeeping; none of them
    /// needs the feature to be armed to be meaningful.
    ///
    /// Errors (rather than silently no-op'ing) when the move does not apply, so
    /// the UI can say why instead of appearing to have worked.
    fn apply_override(
        &self,
        scope: &LatchScope,
        action: LatchOverride,
    ) -> Result<OverrideOutcome, String> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            return Err(format!(
                "no taint latch is engaged for {} — nothing to override",
                scope.label()
            ));
        };
        // An armed one-shot can fire right here: the user restored, ran
        // `/clear`, and the first thing to look at the entry afterwards is their
        // next click. Captured rather than dropped, and recorded below on BOTH
        // exits — a refused action must not swallow a clear that already
        // happened.
        let rotated = entry.observe(scope.session.as_deref());
        let prior = entry.latch;
        let mut prior_taint = None;
        // The action's own verdict, computed before the lock is released so an
        // error path can still record `rotated`.
        let applied = match action {
            // The workflow button: research finished, now apply it. EXTERNAL
            // only — from `Open` there is nothing to flip and from `Local` this
            // would be a no-op that reads like an action. At no instant does the
            // session hold web AND local capability: the flip closes the
            // external side in the same assignment that opens the local one.
            LatchOverride::FlipLocal => {
                if prior != Latch::External {
                    Err(format!(
                        "\"switch to local\" applies only to an EXTERNAL-latched tab ({} is {})",
                        scope.label(),
                        prior.label()
                    ))
                } else {
                    entry.latch = Latch::Local;
                    // #48 (F-23): record WHY the latch is `local`, at the only
                    // site that can know. This is the fact the native-web
                    // refusal is selected on — see
                    // [`TabLatch::local_by_user_flip`] — and it is written under
                    // the same lock as the assignment above so the two cannot be
                    // observed apart.
                    entry.local_by_user_flip = true;
                    Ok(())
                }
            }
            // The at-own-risk button: both sides open again. Valid from any
            // state except a latch that is already open, which would be a
            // no-op.
            //
            // Decision 15's 2026-08-10 amendment: **it also clears the
            // contamination bit.** The trust root is the one that closed H-2 —
            // authority, not evidence. An attacker cannot click this; the click
            // already hands back the strictly more dangerous capability (read +
            // web, with the injected content still in the context window) behind
            // the popover's own confirmation; so leaving persistent memory
            // quarantined afterwards overruled a judgement the product had just
            // asked the user to make. `FlipLocal` above keeps the bit precisely
            // because it is a workflow step and not a verdict.
            //
            // Ordering is load-bearing: the clear runs BEFORE `latch = Open`, so
            // `PriorTaint::latch` records the latch the bit was released from
            // (`external`/`local`) rather than the `open` this arm is about to
            // write — which is what keeps it equal to `OverrideOutcome::prior`,
            // the value `override_row` puts in the same sentence.
            LatchOverride::Unlatch => {
                if prior == Latch::Open {
                    Err(format!(
                        "{} is not latched — nothing to unlatch",
                        scope.label()
                    ))
                } else {
                    // `None` here is not an error: the unlatch is legal on its
                    // own terms and the clear is a consequence of it, not its
                    // purpose. An uncontaminated latched tab unlatches and
                    // writes no `contamination_cleared` row — see
                    // `unlatch_clear_row`.
                    prior_taint = entry.clear_contamination();
                    entry.latch = Latch::Open;
                    // #48 (F-23): the web side is open again, so nothing is being
                    // refused for this reason any more. Cleared here rather than
                    // left to the next rotation because the field must never
                    // outlive the latch position it explains.
                    entry.local_by_user_flip = false;
                    Ok(())
                }
            }
            // Step 4, the false-positive resume. Clears the bit and NOTHING
            // else: the latch stays where it is (it has its own buttons), the
            // budget keeps its spend, the session and the tab are not touched,
            // and quarantined notes stay quarantined.
            //
            // It supersedes an arm, which `clear_contamination` drops — there is
            // nothing left to wait for once the bit is gone.
            LatchOverride::ClearContamination => match entry.clear_contamination() {
                Some(p) => {
                    prior_taint = Some(p);
                    Ok(())
                }
                None => Err(format!(
                    "{} is not flagged as contaminated — nothing to clear",
                    scope.label()
                )),
            },
            // Step 4, the restore arm. Clears nothing now, by user decision: a
            // restore rolls back FILES and cannot remove injected text from the
            // model's context window, so this is the case where clearing
            // immediately is least justified.
            LatchOverride::AwaitSessionClear => {
                if !entry.contaminated {
                    Err(format!(
                        "{} is not flagged as contaminated — there is nothing waiting to clear",
                        scope.label()
                    ))
                } else if entry.awaiting_session_clear {
                    // Not a failure so much as an answer, and the popover shows
                    // it verbatim. Still an error rather than a silent success:
                    // a second click that reported "done" would imply something
                    // new happened.
                    Err(format!(
                        "{} is already waiting for a new session — the contamination flag clears \
                         when one is observed",
                        scope.label()
                    ))
                } else {
                    entry.awaiting_session_clear = true;
                    Ok(())
                }
            }
        };
        // Deliberately NOT touched by ANY move here: the session's spent budget.
        // Letting a click refill the fetch budget would make the budget
        // advisory. (Live-verified 2026-08-10: an unlatch does not refill it —
        // recipe 13's web-side leg could not be re-probed for exactly that
        // reason.)
        //
        // And `contaminated` is not touched by `FlipLocal`: the flip changes
        // what the session may reach next and cannot un-read what the model has
        // already read. `Unlatch` DOES release it — decision 15's 2026-08-10
        // amendment, argued in that arm — and `Unlatch` is the only latch move
        // that does.
        let view = entry.view();
        drop(tabs);
        ContaminationCleared::record_from(rotated, scope);
        applied?;
        warn!(
            target: "offload",
            agent = scope.agent,
            tab = %scope.tab,
            action = action.as_str(),
            prior = prior.label(),
            latch = view.latch,
            contaminated = view.contaminated,
            awaiting_session_clear = view.awaiting_session_clear,
            "loopback: V32 containment state moved by explicit user override"
        );
        Ok(OverrideOutcome {
            prior,
            prior_taint,
            view,
        })
    }

    /// Step 4: fold each known tab's CURRENT live session into its entry, so a
    /// rotation the harness has already proved reaches [`TabLatch::observe`]
    /// even when the tab has made no gated call since.
    ///
    /// **Why the read path needs this.** Before step 4, `observe` ran only from
    /// `gate`, `beacon` and `view_for` — i.e. only when the harness did
    /// something. That was fine when a rotation had no user-visible consequence
    /// worth waiting for. It is not fine now: the whole promise of the restore
    /// arm is "run `/clear` and the flag lifts", and a Claude tab has no
    /// `/latch/state` poll, so without this the flag would sit set until the
    /// model happened to call a cImp tool. This is the read the UI already makes
    /// every 4 s ([`latch_snapshot`]) — no second timer, no new schedule.
    ///
    /// **It grants nothing a call would not have granted anyway.** Everything
    /// `observe` resets is permissive state that the very next gated call would
    /// have reset before deciding anything, so doing it at read time is strictly
    /// a matter of *when* the same fact becomes visible.
    ///
    /// Takes resolved scopes rather than an `AppHandle` so the session lookup
    /// (which locks the graph service) happens outside this lock.
    fn observe_all(&self, scopes: &[LatchScope]) -> Vec<ContaminationCleared> {
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cleared = Vec::new();
        for scope in scopes {
            if let Some(entry) = tabs.get_mut(&scope.key()) {
                if let Some(ev) = entry.observe(scope.session.as_deref()) {
                    cleared.push(ev.into_row(scope));
                }
            }
        }
        cleared
    }

    /// Every `(agent, tab)` the registry holds an entry for. Cloned out under
    /// the lock so [`latch_snapshot`] can resolve live sessions without holding
    /// it.
    fn keys(&self) -> Vec<(&'static str, String)> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        tabs.keys().cloned().collect()
    }

    /// V32 Phase C (locked decision 11): whether this tab's session may make
    /// another EXTERNAL call, and the one-per-scope exhaustion report.
    ///
    /// Runs only on `/mcp/call` — every name that route serves is proxied and
    /// therefore EXTERNAL; `/graph_run` serves cImp-native tools that pull no
    /// external bytes and are not budgeted. Fail-open on a call with no tab
    /// identity, exactly like [`gate`](Self::gate): there is no scope to charge.
    fn budget_gate(
        &self,
        scope: Option<&LatchScope>,
        limits: outbound::BudgetLimits,
        tool: &str,
    ) -> Result<(), &'static str> {
        let Some(scope) = scope else { return Ok(()) };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(entry) = tabs.get_mut(&scope.key()) else {
            // No entry yet ⇒ `gate` has not run for this tab, so nothing is
            // spent. (In practice `gate` always runs first on this route.)
            return Ok(());
        };
        if !entry.budget.exhausted(limits) {
            return Ok(());
        }
        let first = entry.budget.claim_flag();
        drop(tabs);
        if first {
            warn!(
                target: "offload",
                agent = scope.agent,
                tab = %scope.tab,
                tool = %tool,
                "loopback: external fetch budget exhausted for this session"
            );
            outbound::record_flag(outbound::Flag {
                screen: outbound::Screen::Budget,
                origin: outbound::Origin::Internal,
                consumer: scope.agent,
                scope: &scope.label(),
                // The scope is in hand — see `LatchScope::attribution`.
                attribution: scope.attribution(),
                session: scope.session.as_deref(),
                tool,
                host: None,
                url: None,
                resolved_ip: None,
                canary: false,
                root: scope.root.clone(),
                detail: outbound::REFUSAL_BUDGET,
            });
        }
        Err(outbound::REFUSAL_BUDGET)
    }

    /// Charge one completed EXTERNAL call to this tab's session budget.
    /// Silently no-ops without tab identity (nothing to charge) — the same
    /// fail-open the latch takes.
    fn charge(&self, scope: Option<&LatchScope>, response_bytes: usize) {
        let Some(scope) = scope else { return };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            entry.budget.charge(response_bytes);
        }
    }

    /// Charge one **attempted** proxied call, whatever it returned (#48, D-3).
    ///
    /// The charge used to sit on the `Ok` arm only, so a loop of fetches
    /// against a host that 500s advanced neither the byte counter nor the call
    /// counter and never exhausted the budget — while the worker's copy of the
    /// same contract charged both arms (an `Err` there becomes an `ERROR: …`
    /// tool result with `executed = true`). Two paths, one contract, opposite
    /// behaviour.
    ///
    /// A failed fetch charges **zero bytes and one call**: nothing was
    /// ingested, but the request left the machine and `max_calls` is what
    /// exists to stop a loop. Taking the whole decision here — rather than a
    /// `map` at the call site — is what makes it testable: the handler's use is
    /// one unconditional statement above the match it used to be inside.
    /// Generic over the error half since #48 M-17 made it a
    /// `mcp_host::HostError`: this function reads only `is_ok`/`len`, and the byte
    /// charge is unchanged by the error type.
    fn charge_call<E>(&self, scope: Option<&LatchScope>, result: &Result<String, E>) {
        self.charge(scope, result.as_ref().map(|t| t.len()).unwrap_or(0));
    }

    /// Claim one of this tab session's audit-row bits — see
    /// [`outbound::AuditClaims`]. Locks for exactly the length of the claim, so
    /// nothing is held across the SSRF screen's DNS `await`.
    ///
    /// Without a registry entry (no tab identity, or `gate` has not run) there
    /// is no session to attribute a repeat to, so the claim falls back to
    /// `unscoped` — which since #48 F-40 is the identity-less scope's own
    /// process-global ledger ([`outbound::UnscopedAudit`]) and **not** a
    /// constant. The latch and the budget still fail open here; the ROWS no
    /// longer do, because "no session" was never a reason for a caller to be
    /// able to write one row per event into a capped lane.
    ///
    /// `unscoped` is a closure so the fallback ledger is touched only when it is
    /// actually reached, and — load-bearing — so its lock is never taken while
    /// this one is held. The early `return` inside the `if let` is what ends the
    /// registry borrow before that call.
    fn claim<T>(
        &self,
        scope: Option<&LatchScope>,
        claim: impl FnOnce(&mut outbound::Budget) -> T,
        unscoped: impl FnOnce() -> T,
    ) -> T {
        let Some(scope) = scope else { return unscoped() };
        let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = tabs.get_mut(&scope.key()) {
            return claim(&mut entry.budget);
        }
        drop(tabs);
        unscoped()
    }

    /// The `/status` view: one row per tab the proxy has served, sorted so the
    /// output is stable to eyeball across polls.
    fn snapshot(&self) -> Vec<LatchStatus> {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<LatchStatus> = tabs
            .iter()
            .map(|((agent, tab), st)| LatchStatus {
                consumer: agent,
                tab: tab.clone(),
                session: st.session.clone(),
                view: st.view(),
            })
            .collect();
        rows.sort_by(|a, b| (a.consumer, &a.tab).cmp(&(b.consumer, &b.tab)));
        rows
    }
}

/// One `/status` latch row.
#[derive(Serialize, Debug)]
pub struct LatchStatus {
    pub consumer: &'static str,
    pub tab: String,
    pub session: Option<String>,
    /// V32 Phase F: the latch label plus the contamination bit and per-row
    /// override availability. **Flattened**, so the wire shape is unchanged for
    /// the Phase B readers (`latch` stays a top-level key of the row) and the
    /// new facts sit beside it rather than in a nested object — one row per
    /// tab, as `/status` has always been.
    #[serde(flatten)]
    pub view: LatchView,
}

impl LatchStatus {
    /// [`Latch::label`] for this row: `open` / `external` / `local`.
    ///
    /// Read by the tests (and by anyone holding a snapshot); the wire form goes
    /// through `view`'s flattened `latch` key, so the running app never calls
    /// this — the same `cfg_attr` shape `toolclass::mutates_fs` carried until
    /// V33 Phase F landed its consumer and removed it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn latch(&self) -> &'static str {
        self.view.latch
    }
}

/// The process-wide registry. Latch state is intentionally in-memory and
/// non-durable: it describes *live* conversations, and an app restart
/// necessarily ends every one of them.
///
/// **It is bounded** — one entry per (agent, tab) pair, over the AI tab ids the
/// user has configured, which are reused across restarts, so nothing
/// accumulates over a long-running app. That was asserted here and enforced
/// nowhere until #45: every key arrives in a request body, and the map has no
/// TTL, no cap and no eviction, while every entry is serialized into every
/// `/status` response and every 4 s `latch_status` poll. The bound is now real
/// and tested — [`is_configured_tab`], applied in [`latch_scope`], which is the
/// one funnel through which `gate` and `beacon` (the only two methods that
/// insert) receive a scope at all.
///
/// The caveat that keeps this honest: the bound is `configured AI tabs × 1`
/// only while the settings snapshot is readable. See [`is_configured_tab`]'s
/// empty-list escape.
fn latches() -> &'static LatchRegistry {
    static LATCHES: OnceLock<LatchRegistry> = OnceLock::new();
    LATCHES.get_or_init(LatchRegistry::default)
}

/// One tab session's audit-row claim ledger, as the SSRF chokepoint and the
/// detection boundary see it (#48).
///
/// The ledger itself lives inside the tab's [`outbound::Budget`], which is the
/// only per-conversation state with the right lifetime *and* the right reset
/// rule: `TabLatch::observe` wipes it on a proved session rotation, so a
/// genuinely new conversation is entitled to its own rows. A process-global
/// `HashSet<scope>` was the alternative and is wrong for exactly that reason —
/// proxy scopes are stable `agent:tab` strings, so it would suppress a tab's
/// rows permanently, across every session it ever holds.
///
/// A handle rather than a borrow because the ledger sits behind the registry
/// mutex, which must not be held across the SSRF screen's DNS `await`.
///
/// The second field is the agent whose identity-less ledger a call **without** a
/// scope claims against ([`outbound::UnscopedAudit`], #48 F-40). It is carried
/// rather than derived because at the one construction site the agent is already
/// resolved through `graph::source_for_consumer` — the same normalisation that
/// builds the `agent:(no tab identity)` label the resulting row shows, so the
/// ledger and the label cannot name different things.
struct TabAudit<'a>(Option<&'a LatchScope>, &'static str);

impl TabAudit<'_> {
    /// This call's fallback ledger. One place, so both claims agree on it.
    fn unscoped(&self) -> outbound::UnscopedAudit {
        outbound::UnscopedAudit::for_agent(self.0.map(|s| s.agent).unwrap_or(self.1))
    }
}

impl outbound::ScopeAudit for TabAudit<'_> {
    fn claim_ssrf(&self) -> outbound::DoublingRow {
        latches().claim(self.0, outbound::Budget::claim_ssrf_flag, || {
            self.unscoped().claim_ssrf()
        })
    }
    fn claim_unscreened(&self) -> bool {
        latches().claim(self.0, outbound::Budget::claim_unscreened_flag, || {
            self.unscoped().claim_unscreened()
        })
    }
}

/// V32 Phase F: the `/status` latch rows, read **in process**.
///
/// The per-tab taint badge and its override popover live in the webview, which
/// has no bearer token and no business acquiring one — every loopback route is
/// authenticated precisely so only cImp-spawned children can reach it. The
/// Tauri backend already owns the registry, so the UI goes through an IPC
/// command ([`crate::ipc::commands::latch_status`]) that calls this, and the
/// token never leaves the processes that need it.
///
/// **Step 4: it folds each tab's current live session in first.** See
/// [`LatchRegistry::observe_all`] for why the read path has to — a Claude tab
/// polls no `/latch/state`, so without this an armed one-shot would wait for the
/// model to call a cImp tool rather than for the user's `/clear`. This is the
/// same 4 s read the badge already makes; no second timer is introduced.
pub fn latch_snapshot(app: &AppHandle) -> Vec<LatchStatus> {
    // Resolve scopes with the registry lock NOT held: `latch_scope` locks the
    // graph service for the live-session lookup.
    let settings = live_settings(app);
    let scopes: Vec<LatchScope> = latches()
        .keys()
        .iter()
        .filter_map(|(agent, tab)| {
            latch_scope(app, &settings, agent, Some(tab.as_str())).into_scope()
        })
        .collect();
    for cleared in latches().observe_all(&scopes) {
        cleared.record();
    }
    latches().snapshot()
}

/// The caller-composed parts of one Phase F `injection_flag` row: its
/// provenance, its `tool` column and the prose an incident reviewer reads.
///
/// **They are built together on purpose (#48).** #45 shipped them apart — the
/// detail functions spelled `Origin::Ipc` / `Origin::Http` into their own
/// format strings while `Flag.origin` was set independently at the call site.
/// #47 then made `origin` a required field precisely so provenance could not be
/// taken by omission, but the sentence a human actually reads was still not
/// derived from it: re-expose an HTTP path into the override and the row's
/// `origin` key would say `http` while its text went on asserting that a human
/// clicked, with nothing to catch it. One struct, one origin, both consumers.
struct FlagRow {
    /// Which feed lane the row belongs in. Carried here since step 4, because
    /// one of the four override actions is not a latch move at all — it releases
    /// the contamination bit, and that belongs in
    /// [`outbound::Screen::ContaminationCleared`] beside the row that SET the
    /// bit rather than among the latch moves. Deciding it here keeps the choice
    /// beside the sentence that describes it.
    screen: outbound::Screen,
    /// Copied verbatim into [`outbound::Flag::origin`], and interpolated into
    /// [`detail`](Self::detail) by the same function that received it.
    origin: outbound::Origin,
    /// The row's at-a-glance `tool` column.
    tool: String,
    /// The row's human-readable body.
    detail: String,
}

/// An override's `injection_flag` row (#45), composed from the origin the
/// caller states rather than one baked in here (#48).
///
/// Split out of [`apply_latch_override`] so the row's content is assertable
/// without an `AppHandle`, which this crate has no mock for — every Phase F
/// test called [`LatchRegistry::apply_override`] directly and stopped short of
/// the row, leaving the one artifact an incident review actually reads
/// uncovered.
fn override_row(
    origin: outbound::Origin,
    action: LatchOverride,
    outcome: &OverrideOutcome,
) -> FlagRow {
    // The action is the row's at-a-glance "tool" for the three latch-shaped
    // moves: these rows have no tool call behind them, and what the user DID is
    // the fact worth reading. The clear names its own basis instead.
    let (screen, tool) = match action {
        LatchOverride::ClearContamination => (
            outbound::Screen::ContaminationCleared,
            ClearBasis::Resume.tool().to_string(),
        ),
        _ => (outbound::Screen::LatchOverride, action.as_str().to_string()),
    };
    let detail = match action {
        // Step 4: composed by the SAME function the armed-rotation clear uses,
        // so the two paths cannot describe one state change two ways.
        LatchOverride::ClearContamination => clear_detail(
            ClearBasis::Resume,
            origin,
            outcome
                .prior_taint
                .as_ref()
                .map_or(outcome.prior.label(), |p| p.latch),
            outcome
                .prior_taint
                .as_ref()
                .and_then(|p| p.session.as_deref()),
            None,
        ),
        // Step 4: the arm. It clears nothing, and the row has to say so — a
        // reader who sees "restore" in the feed and no `contamination_cleared`
        // row afterwards must be able to tell "still waiting" from "lost".
        LatchOverride::AwaitSessionClear => format!(
            "USER OVERRIDE (await_session_clear, origin: {}): a checkpoint was restored for this \
             tab, and the contamination flag is deliberately NOT cleared (contaminated={}). \
             Restoring rolls back FILES; it cannot remove injected text from the model's context \
             window, so this is the case where clearing immediately would be least justified. cImp \
             will clear the flag when it observes this tab start a new harness session — run \
             `/clear` in the tab, or restart it. Until then memory writes stay quarantined and \
             external results keep their envelope. Latch unchanged ({}).",
            origin.as_str(),
            outcome.view.contaminated,
            outcome.view.latch,
        ),
        // The flip is a WORKFLOW step, not a verdict, which is the whole reason
        // decision 15's 2026-08-10 amendment narrowed "contamination outlives
        // the override" to this one action. The row is where a reviewer learns
        // which of the two moves they are looking at.
        LatchOverride::FlipLocal => format!(
            "USER OVERRIDE (flip_local, origin: {}): taint latch {} → {}. Contamination is NOT \
             cleared by the flip (contaminated={}): memory writes stay quarantined and external \
             results keep their envelope, because the injected content is still in the \
             conversation and \"switch to local\" says \"research done, now apply it\" — not \"that \
             content was harmless\". Clearing the flag is its own decision with its own three \
             actions: `clear_contamination` (the user judges the content harmless), `unlatch` (the \
             user restores FULL access and accepts the larger risk) and `await_session_clear` \
             (after a restore, effective once a new harness session is observed). No automatic \
             path and no HTTP route can reach any of them.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            outcome.view.contaminated,
        ),
        // Decision 15's 2026-08-10 amendment. One click, two effects, and the
        // row states both — including whether the second one actually fired: an
        // unlatch on an uncontaminated tab clears nothing, and this sentence
        // must not be readable as evidence that a bit was released.
        LatchOverride::Unlatch => format!(
            "USER OVERRIDE (unlatch, origin: {}): taint latch {} → {} — FULL access restored, \
             which recreates the read+web trifecta with any injected content still in the \
             conversation. {} Memory notes ALREADY quarantined STAY quarantined — promoting or \
             discarding them is the Memory view's own review (locked decision 10), a separate \
             consent surface.",
            origin.as_str(),
            outcome.prior.label(),
            outcome.view.latch,
            match outcome.prior_taint.as_ref() {
                Some(p) => format!(
                    "The contamination flag was cleared by the same click (prior state: \
                     contaminated=true, latch={}, session={}), and it is filed as its own \
                     `contamination_cleared` row beside the row that SET the bit. The trust root \
                     is AUTHORITY, not evidence: an attacker cannot click this, and the click \
                     already handed back the strictly more dangerous capability — so leaving \
                     persistent memory writes quarantined would have overruled a judgement the \
                     product had just asked the user to make. This tab's future `context_note` \
                     writes are stored clean again, and a fresh contamination will report itself \
                     as a new transition.",
                    p.latch,
                    p.session.as_deref().unwrap_or("unknown"),
                ),
                None => format!(
                    "This tab was not flagged as contaminated, so there was nothing to clear \
                     (contaminated={}).",
                    outcome.view.contaminated
                ),
            },
        ),
    };
    FlagRow {
        screen,
        origin,
        tool,
        detail,
    }
}

/// Decision 15's 2026-08-10 amendment: the `contamination_cleared` row a **full
/// unlatch** owes, or `None` when this override released nothing.
///
/// # Why a second row rather than a sentence in the first one
///
/// [`outbound::Screen::ContaminationCleared`] is a retention lane *and* a join
/// key: its own doc says a reviewer filtering the two contamination wire values
/// "gets one tab's whole taint lifecycle", and [`outbound::contamination_events`]
/// queries exactly those two lanes. A release visible only inside a
/// [`outbound::Screen::LatchOverride`] detail string is invisible to that join —
/// the Workbench Timeline would show a `☣` that never closes, for a tab the
/// registry reports clean. That is the "signal with no consumer" class (#48,
/// F-3) reintroduced one amendment later, so the clear is filed where every
/// other clear is filed.
///
/// # Why it is composed here and not in [`LatchRegistry::apply_override`]
///
/// The origin is stated ONCE, by the caller (#48, A2-3): [`apply_latch_override`]
/// is the only path in and it names [`outbound::Origin::Ipc`] for both rows, so
/// the two halves of an override cannot disagree about who acted. Composing it
/// here also makes it assertable without an `AppHandle`, which this crate has no
/// mock for — the same seam [`override_row`] exists for.
///
/// `None` covers the honest case: an unlatch on a tab that was never
/// contaminated. It is not an error, and it must not write a row saying a bit
/// was released.
fn unlatch_clear_row(
    origin: outbound::Origin,
    action: LatchOverride,
    scope: &LatchScope,
    outcome: &OverrideOutcome,
) -> Option<ContaminationCleared> {
    if action != LatchOverride::Unlatch {
        return None;
    }
    let prior = outcome.prior_taint.as_ref()?;
    Some(ContaminationCleared {
        origin,
        basis: ClearBasis::Unlatch,
        consumer: scope.agent,
        scope: scope.label(),
        // The conversation the bit was cleared FOR, exactly as the resume path
        // files it: the one the `contamination` row named.
        session: prior.session.clone(),
        root: scope.root.clone(),
        detail: clear_detail(
            ClearBasis::Unlatch,
            origin,
            prior.latch,
            prior.session.as_deref(),
            None,
        ),
    })
}

/// V32 Phase F (locked decision 15): apply a user-initiated latch move to one
/// tab, write its `injection_flag` row, and return the tab's new view.
///
/// **Reachable from the `latch_override` Tauri IPC command only** — the badge
/// popover, i.e. the user. `POST /latch/override` existed alongside it until
/// #45 "so the same action is reachable from a child or a live-verification
/// script"; that convenience made a capability GRANT drivable by anything
/// holding the launch token, and left the resulting row indistinguishable from
/// a click. There is no HTTP path into this function now, so
/// [`outbound::Origin::Ipc`] on the row is a fact rather than an assumption.
pub fn apply_latch_override(
    app: &AppHandle,
    consumer: &str,
    tab: &str,
    action: &str,
) -> Result<LatchView, String> {
    let action = LatchOverride::parse(action)?;
    let agent = crate::graph::source_for_consumer(consumer);
    // One settings snapshot, shared with the tab-id check inside `latch_scope`.
    let settings = live_settings(app);
    let scope = latch_scope(app, &settings, agent, Some(tab))
        .into_scope()
        .ok_or_else(|| {
            // #45 folded "not a configured tab" into this refusal, so the
            // message has to cover both — a popover that said "needs a tab id"
            // about a tab id it was given would send the user looking in the
            // wrong place.
            format!("a latch override needs a configured tab id (got {tab:?})")
        })?;
    let outcome = latches().apply_override(&scope, action)?;

    // Locked decision 15: "every override writes an `injection_flag` row … so
    // the feed records who opened what." `ok: true` — nothing was denied; this
    // is a capability GRANT, and the feed must show it as the deliberate act it
    // is rather than as a failure. The prior latch is in the detail because
    // "restored full access" from `external` and from `local` are very
    // different events.
    //
    // The origin is stated ONCE, here (#48): `override_row` puts it in the
    // row's `origin` key and in the sentence the reviewer reads, so the two
    // cannot come apart. `Ipc` is the one origin that means a human acted, and
    // it is a fact rather than an assumption only because no HTTP path into
    // this function survives (#45) — re-expose one and this constant is what
    // has to change.
    let row = override_row(outbound::Origin::Ipc, action, &outcome);
    outbound::record_flag(outbound::Flag {
        // Step 4: the row's own screen, not a constant here — a contamination
        // clear is filed beside the row that set the bit, not among the latch
        // moves. See `FlagRow::screen`.
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
    // Decision 15's 2026-08-10 amendment: a full unlatch also RELEASES the
    // contamination bit, and that release owes the `contamination_cleared` lane
    // its own row — see `unlatch_clear_row` for why it is not folded into the
    // latch move's prose. Written AFTER the override row, in the order the state
    // moved: the latch reopened, and the flag went with it. Same stated origin
    // for both, from the one constant above.
    if let Some(cleared) = unlatch_clear_row(outbound::Origin::Ipc, action, &scope, &outcome) {
        cleared.record();
    }
    Ok(outcome.view)
}

/// `POST /graph_run`: run one `graph_*` tool against the app's WARM graph index
/// (single shared connection — no second cross-process open of the SQLite store)
/// and return its text. The `GraphService` is resolved from managed state at
/// request time, so this is robust against the graph-vs-loopback startup order.
async fn handle_graph_run(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let body: GraphRunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let graph = match app.try_state::<Arc<crate::graph::GraphService>>() {
        Some(g) => g.inner().clone(),
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("graph service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let consumer = body.consumer.as_deref().unwrap_or("claude");
    // V28: resolve the calling TAB to the session it currently reports, so the
    // `context_*` memory tools scope to this tab's own session instead of "the
    // most recent session for this agent" (two same-agent tabs used to share —
    // and steal — one memory scope). Fail-open: no `tab`, an unknown key, a
    // different agent's entry, or a TTL-stale one all yield `None`, which is
    // exactly the pre-V28 behavior.
    //
    // V32 Phase B folds that same resolution into the latch scope, so the
    // registry is consulted once and the memory scope and the taint scope can
    // never disagree about which session this call belongs to.
    //
    // V32 Phase G: ONE settings read for the whole call (see the sibling note
    // on `/mcp/call`). #45 pulls it above the scope resolution, because the tab
    // id is now validated against the configured tab list and that check must
    // use the same snapshot as the policy it feeds.
    let settings = live_settings(app);
    let scoping = latch_scope(
        app,
        &settings,
        crate::graph::source_for_consumer(consumer),
        body.tab.as_deref(),
    );
    // #48 F-20: resolved BEFORE `into_scope()` collapses `Anonymous` and
    // `Unknown` into one `None`. That collapse is right for the latch (both fail
    // open) and wrong for the row, which has to say which of the three this call
    // was. See `LatchScoping::attribution`.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let session = scope.as_ref().and_then(|s| s.session.clone());

    // V32 Phase B: the session taint latch over the tools THIS route serves —
    // the content-bearing graph tools (LOCAL-CAPABILITY), the structural ones
    // and the memory reads (TRUSTED, never gated), and `context_note`
    // (PERSISTENT-WRITE).
    //
    // V32 Phase C2 (locked decision 10): a `context_note` under an EXTERNAL
    // latch is no longer refused. The gate returns `Quarantined` and the write
    // proceeds with that verdict threaded into it — the note is stored with a
    // `tainted` flag, kept out of `context_recall`/`context_notes`/the
    // compaction carry-over/the fact distiller (and so out of auto-injection),
    // and held for explicit user promote-or-discard. That preserves the
    // legitimate research conclusion the Phase B refusal dropped.
    //
    // V32 Phase G: both halves resolve through the three-level hierarchy at this
    // tab's scope, from ONE settings read — so a tab with the latch overridden
    // off still quarantines, and a tab with the master switch off does neither.
    // #48 F-16: resolved here, from the service, so the quarantine row this gate
    // may write and the activity row the dispatch will write name the SAME
    // project. `graph_root_key` is `run_graph_tool`'s own resolution, exposed.
    let call_root = graph.graph_root_key(&cwd);
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    let taint = match latches().gate(
        scope.as_ref(),
        LatchRoute::Native,
        &body.name,
        gate_policy,
        CallProvenance::internal_in(&call_root),
    ) {
        Ok(t) => t,
        Err(refusal) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(refusal.to_string()),
            };
            // 200, like every other tool-level error here: the child renders
            // `error` as the tool result, which is how the model reads it.
            return write_json(stream, 200, &r).await;
        }
    };

    let r = match graph
        .run_graph_tool(
            &cwd,
            &body.name,
            &body.args,
            consumer,
            session.as_deref(),
            // V32 Phase G: the second resolved verdict this route carries — the
            // memory-read tools it serves (`context_recall` / `context_notes`)
            // are the recall envelope's delivery point, and only this frame
            // knows the tab whose scope decides it.
            toolclass::CallGuards {
                taint,
                spotlight_recall: crate::settings::injection::effective(
                    crate::settings::injection::Feature::Spotlighting,
                    crate::settings::injection::Scope::for_tab(
                        crate::graph::source_for_consumer(consumer),
                        body.tab.as_deref(),
                    ),
                    &settings,
                ),
            },
            tab_attr,
        )
        .await
    {
        Ok(text) => RunResult {
            ok: true,
            text: Some(text),
            error: None,
        },
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    // 200 even on a tool-level error: the child renders `error` as a tool result.
    write_json(stream, 200, &r).await
}

/// A `POST /audit/run` request body (V26 code-audit MCP surface).
///
/// Deliberately tiny: `category` reuses
/// [`Category`](crate::audit::adapters::Category)'s own lowercase serde (so
/// `"security"` / `"quality"` deserialize directly — a bad word is a clean parse
/// error → 400). Everything else is validated *after* the parse, by
/// [`audit_admit`], so a bad value becomes a readable tool error rather than a
/// bare 400 the model cannot act on.
#[derive(Deserialize)]
struct AuditRunBody {
    category: crate::audit::adapters::Category,
    /// The agent that triggered the scan, from the child's `--consumer` flag.
    /// It selects which `expose_*` toggle the route re-enforces at run time, so
    /// it is a *capability selector*, not a label — H-8: narrowed to
    /// [`AUDIT_CONSUMERS`] by [`audit_consumer`] before it reaches
    /// [`AuditState::consumer_exposed`](crate::audit::AuditState::consumer_exposed).
    #[serde(default)]
    consumer: Option<String>,
    /// The child's working directory (the agent's project), sent for
    /// verification only — the scan always runs against this app's own
    /// launch root. `#[serde(default)]` keeps older children compatible.
    #[serde(default)]
    cwd: Option<String>,
    /// V32 C-1b (2026-08-07 review): the cImp tab this child serves
    /// (`cimp --code-audit-mcp --tab <id>`), resolved to the tab's taint latch
    /// so a contaminated conversation cannot run a local scanner.
    ///
    /// H-8 (2026-08-08 re-review): **required in practice.** It stays
    /// `Option` on the wire — a missing field must produce the route's readable
    /// refusal, not serde's 400 — but [`audit_tab`] refuses a body without it.
    /// The fail-open anonymous scope the other identity-taking routes keep is
    /// not available here: this route's whole gate keys on tab identity, so
    /// "no tab" meant "no containment", silently.
    #[serde(default)]
    tab: Option<String>,
}

/// The consumers that legitimately POST `/audit/run` (H-8).
///
/// Empirically the complete set — there are exactly two spawn sites for the
/// `cimp --code-audit-mcp` child, and no other caller exists:
///
/// - Claude: `tabs::config::build_pre_args` emits
///   `[--code-audit-mcp, --tab, <id>]` with **no** `--consumer`, so the child's
///   own default (`audit::mcp::CONSUMER`, `"claude"`) goes on the wire. Pinned
///   by `tabs::config::tests::the_code_audit_child_carries_its_own_tab_id`.
/// - OpenCode: `tabs::config::build_opencode_config` emits
///   `[<exe>, --code-audit-mcp, --consumer, opencode, --tab, <id>]`.
///
/// `"offload"` is deliberately **absent**, even though
/// [`CodeAuditSettings::expose_offload`](crate::settings::schema::CodeAuditSettings)
/// exists: the offload worker is an *in-process* consumer of the audit surface
/// and never speaks to this route. `offload::tools::audit_tools::execute` calls
/// [`audit::mcp::run_audit`](crate::audit::mcp::run_audit) directly through
/// `audit::global()`, gated by `OffloadService::run_on` (`enabled` AND
/// `expose_offload` AND a local backend) and re-gated by `HostRouter::call`.
/// `CodeAuditSettings::mcp_exposed` states the same split from the other side
/// ("`expose_offload` is deliberately absent: the offload worker runs
/// in-process"). So `expose_offload` — which defaults **true** — was reachable
/// over HTTP only by a caller that no legitimate component ever is.
const AUDIT_CONSUMERS: [&str; 2] = ["claude", "opencode"];

/// H-8: narrow `/audit/run`'s caller-asserted `consumer` to [`AUDIT_CONSUMERS`]
/// at the parse boundary, returning the `&'static str` the rest of the route
/// uses. Same discipline `ada4bae` gave `/run`'s `tool` label
/// ([`offload_tool_name`]) and for a stronger reason: `tool` is only a label,
/// whereas this value **selects which `expose_*` toggle is checked**.
///
/// Absent/blank still means `"claude"`, which is the child's own default and
/// the pre-H-8 documented behaviour; only *unrecognized* values are refused.
/// (A child old enough to omit `consumer` predates `--tab` and is already
/// refused by [`audit_tab`], so the default is compatibility, not a hole.)
fn audit_consumer(raw: Option<&str>) -> Result<&'static str, String> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude");
    let lower = raw.to_ascii_lowercase();
    AUDIT_CONSUMERS
        .iter()
        .copied()
        .find(|c| *c == lower)
        .ok_or_else(|| {
            format!(
                "code audit does not serve the consumer {raw:?} — this route serves the \
                 cimp-code-audit MCP child only (claude, opencode)"
            )
        })
}

/// H-8: `/audit/run` requires a tab identity — a request without one is
/// **refused**, never treated as clean.
///
/// Both spawn paths have sent `--tab` since V32 C-1b (see [`AUDIT_CONSUMERS`]),
/// so the only bodies this rejects are a hand-run child, a forged request, and
/// a *stale* child left over from a pre-C-1b build — which is why the message
/// names the remedy (restart the tab) rather than the symptom.
///
/// Trimming/emptiness is checked here rather than left to [`tab_identity`]
/// because `""` and `"   "` are exactly the shapes a caller would use to opt
/// itself back out of the gate.
fn audit_tab(raw: Option<&str>) -> Result<&str, String> {
    raw.map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| {
        "this code-audit MCP connection carries no cImp tab id, so the scan cannot be checked \
         against that tab's containment latch — restart this tab in cImp (its MCP child is from \
         an older build) and try again."
            .to_string()
    })
}

/// Everything `/audit/run` decides **before** the scan starts — all four
/// refusals, in the one order they may be taken — returning the validated
/// consumer on success. `Err(msg)` is the single `RunResult { ok: false, error }`
/// the route writes over HTTP 200, always *before* [`write_ndjson_head`].
///
/// It is one function, taking its dependencies as arguments, for two reasons:
/// the caller cannot reach [`LatchRegistry::gate`] without passing every check
/// first (so an added refusal cannot be inserted on the wrong side of the
/// gate), and the ordering below is testable without a `TcpStream` or an
/// `AppHandle`.
///
/// **Why this order** (each step's own note says what it decides):
///
/// 1. `consumer` — H-8. A *parse-boundary narrowing*: it must precede step 2
///    because it is what step 2 is keyed by. Engages nothing.
/// 2. `expose` — the per-consumer run-time re-gate. Kept ahead of the identity
///    and containment checks so a consumer the user has opted out still gets
///    the specific "not exposed" error rather than a containment refusal that
///    would not explain its situation. Engages nothing.
/// 3. `cwd` — the wrong-instance guard. Same reasoning: a misrouted request was
///    never going to run here, and its own error is the actionable one.
///    Engages nothing.
/// 4. `tab` — H-8. The identity half of the gate below, so it sits immediately
///    before it and shares its "a request that was never going to run does not
///    engage this tab's latch" property. Engages nothing: the refusal happens
///    before any [`LatchScope`] exists.
/// 5. the taint gate — the only step that may touch the registry, and therefore
///    last, exactly as V32 C-1b established.
fn audit_admit(
    reg: &LatchRegistry,
    body: &AuditRunBody,
    served_root: &Path,
    exposed: impl FnOnce(&str) -> bool,
    scope_of: impl FnOnce(&'static str, &str) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<&'static str, String> {
    // 1. H-8: the caller-asserted `consumer` is narrowed to one of two known
    //    values before anything reads it — see `audit_consumer`.
    let consumer = audit_consumer(body.consumer.as_deref())?;

    // 2. Re-enforce this consumer's expose toggle at run time (see the route's
    //    doc comment): a still-registered child whose consumer has since been
    //    opted out gets a clean tool error, not a scan.
    if !exposed(consumer) {
        return Err(format!(
            "code audit is not exposed to {consumer} — re-enable it in cImp Settings → Code Audit"
        ));
    }

    // 3. Wrong-instance guard: the scan always runs against THIS app's launch
    //    root, so a child whose cwd falls outside it was misrouted (stale or
    //    foreign discovery entry — possible with several cImp instances off one
    //    install). A clean error beats silently auditing the wrong project.
    //
    //    **This is a routing guard, not a boundary, and must not be read as
    //    one** (V33, recorded so the asymmetry with `/context/post_edit` is not
    //    re-raised): `cwd` is `#[serde(default)]` for older children, so step 3
    //    is skipped outright when the field is absent — a caller holding the
    //    loopback token opts out of it by saying nothing. Nothing is gained by
    //    doing so either: passing this check does not choose what gets scanned;
    //    `served_root` does, and it comes from the app. What the two routes DO
    //    share is the path comparison itself, and its `..` refusal now lives in
    //    `is_ancestor_or_equal` so neither route can drift from the other.
    if let Some(child_cwd) = body.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let served = canon(served_root);
        if !is_ancestor_or_equal(&served, &canon(Path::new(child_cwd))) {
            return Err(format!(
                "this cImp instance serves {} — launch cImp in {} (or close the other instance) to audit it",
                served.display(),
                child_cwd
            ));
        }
    }

    // 4. H-8: identity is REQUIRED here. Before this, `tab` was caller-supplied
    //    and optional, absence resolved to `LatchScoping::Anonymous`, and
    //    `gate(None, ..)` returned `Ok(Clean)` without classifying anything —
    //    i.e. the whole containment gate below was opt-in by the caller it was
    //    meant to contain, and opting out was silent.
    let tab = audit_tab(body.tab.as_deref())?;

    // 5. V32 C-1b: the taint gate — the last thing checked before the scan
    //    starts. Identity is resolved through the same `latch_scope` funnel
    //    every other gated route uses (`scope_of`) rather than a route-local
    //    check that can drift from it, and an unknown tab id still yields no
    //    scope and so keys no registry entry: #45's bound.
    let tool = crate::audit::mcp::tool_name_for(body.category);
    let scoping = scope_of(crate::graph::source_for_consumer(consumer), tab);
    let scope = scoping.scope();
    if scope.is_none() {
        // H-8: **a containment gate that does not apply is never silent.** The
        // surviving no-scope case is `Unknown` (an id naming no configured tab
        // — a re-id'd or removed tab, or a forged id); `Anonymous` is refused
        // at step 4 and can no longer arrive here, but this is written over
        // `scope.is_none()` rather than over the `Unknown` variant so that a
        // future variant, or a regression in step 4, still warns instead of
        // passing through unremarked.
        //
        // Not a refusal: refusing here would break a running child whose tab
        // was re-id'd under it, and V28's honest fallback for "an identity we
        // cannot resolve" on a TOOL route is fail-open. But it is the case
        // where containment does not apply, so it is stated in the log rather
        // than left to be inferred from a missing row.
        warn!(
            target: "offload",
            consumer = %consumer,
            tab = %tab,
            tool = %tool,
            "loopback: /audit/run has no configured tab to latch against — scan is ungated"
        );
    }
    let policy = policy_of(scope);
    reg.gate(
        scope,
        LatchRoute::Native,
        tool,
        policy,
        CallProvenance::internal(),
    )
    .map(|_| consumer)
    .map_err(str::to_string)
}

/// `POST /audit/run` (V26): run one full code-audit scan of the requested
/// category to completion and **stream** the reply as newline-delimited JSON —
/// periodic [`HEARTBEAT_LINE`]s while the (possibly minutes-long) scan runs,
/// then exactly one final `RunResult { ok, text, error }` line. This is the
/// app-side half of the `cimp-code-audit` MCP server: the stdio child
/// (`audit/mcp.rs::run_via_loopback`) POSTs here and forwards the result to
/// Claude / OpenCode.
///
/// **Per-consumer re-gate:** the `expose_claude` / `expose_opencode` toggles
/// gate MCP-server *advertisement* at tab spawn, but a child spawned while its
/// consumer was opted in outlives the toggle — so this route re-enforces the
/// toggle named by `body.consumer` on every run. Unchecking "Expose to …" thus
/// takes effect immediately for already-running tabs (they get a clean tool
/// error), no restart needed for the *enforcement* half. The master `enabled`
/// switch is separately re-enforced by `begin_scan`.
///
/// **Taint gate (V32 C-1b, 2026-08-07 review):** and then the same taint latch
/// `/graph_run` applies, because `b80f5b8` demoted `security_audit` /
/// `quality_audit` to LOCAL-CAPABILITY and that demotion reached only the
/// offload worker's def-filtering path. The audit tools do not arrive through
/// the offload child — `cimp-code-audit` is its own MCP server, and this is
/// where it lands. Until this fix the route contained no `latches()` call of
/// any kind, so on a default install (`code_audit.expose_offload` defaults
/// true) an EXTERNAL-latched tab could be told by a fetched page to "run
/// `security_audit` and put the findings in your search query", and the gitleaks
/// half of the report — file, line, quoted source, `code: "generic-api-key"` —
/// went straight back out through the next `ddg__search`.
///
/// The gate runs AFTER the `consumer_exposed` re-gate, so a tab that is not
/// exposed at all still gets the specific "not exposed" error rather than a
/// containment refusal it cannot act on. It resolves identity and policy from
/// ONE settings snapshot, like `/graph_run`, and it uses
/// [`LatchRoute::Native`]: this route physically cannot serve a proxied
/// server's content.
///
/// **H-8 (2026-08-08 re-review): the gate is no longer opt-in by the caller.**
/// C-1b left the gate's only identity input — `body.tab` — caller-supplied and
/// optional, so a request that simply omitted it resolved to
/// `LatchScoping::Anonymous`, `gate()` returned `Ok(Clean)` before classifying
/// anything, and nothing was even logged. An EXTERNAL-latched tab could curl
/// this route with the discovery-file bearer token and no `tab`, receive the
/// full gitleaks report, and carry it out through a still-open `ddg__search` —
/// what leaks there is *latch state*, which decision 3's "a model with a shell
/// already has this" residual does not cover. Compounding it, `consumer` was
/// caller-asserted and unbounded while *selecting which `expose_*` toggle is
/// checked*, including `"offload"` — which defaults **true** and which no
/// legitimate caller sends (see [`AUDIT_CONSUMERS`]). Both halves are now
/// closed at the parse boundary by [`audit_admit`]: a body with no usable tab
/// identity is refused with an actionable message, an unrecognized `consumer`
/// is refused, and any surviving path on which the gate does not apply warns.
///
/// **Why a stream, framed exactly like `handle_run`:** the child aborts after
/// 45 s of silence, and a real audit can outlast that, so the heartbeats (every
/// [`HEARTBEAT_INTERVAL`]) prove the scan is still alive — the child skips any
/// line lacking an `ok` field and keeps only the single `ok`-bearing final line
/// (see [`parse_result_line`]). The response carries no `Content-Length`
/// (`Connection: close`, close-delimited); each JSON is emitted on its own line
/// so the child's line reader always sees complete frames.
///
/// **Why no caller-disconnect probe (unlike `handle_run`):** the audit entry
/// [`run_audit`](crate::audit::mcp::run_audit) is not cancellable, and
/// `run_scan_and_wait` clears the runner's `scanning` flag only when the scan's
/// `run()` future completes. Dropping that future to react to a disconnect would
/// wedge the runner in `scanning`, so instead a failed heartbeat write (the
/// caller-gone signal) drains the scan to completion off the wire and discards
/// the unsendable result — the runner ends clean either way. There is also no
/// llama-server slot to free promptly, which is the only reason `handle_run`
/// probes at all.
///
/// Tool-level failures (master switch off, no tools enabled, `"a scan is already
/// in progress"`) flow through as the final `{ok:false, error}` line over HTTP
/// 200 — the child renders them as a readable tool error, mirroring
/// [`handle_graph_run`]. Only a malformed body is a 400.
async fn handle_audit_run(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let body: AuditRunBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            // Malformed body / unknown category → 400 (the child treats any
            // non-200 as a hard failure), mirroring `handle_graph_run`.
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let category = body.category;

    // Resolve the runner from managed state at request time (robust to the
    // audit-vs-loopback startup order). `main.rs` manages it as `Arc<AuditState>`
    // (and publishes the same handle via `audit::set_global`). Not ready → a
    // single `ok:false` line over 200, same shape as `handle_graph_run`.
    let state = match app.try_state::<Arc<crate::audit::AuditState>>() {
        Some(s) => s.inner().clone(),
        None => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some("audit service not ready".into()),
            };
            return write_json(stream, 200, &r).await;
        }
    };

    // Every pre-scan check, in the one order they may be taken, over ONE
    // settings read for identity + policy (the `/mcp/call` discipline) — see
    // [`audit_admit`], which owns the ordering rationale and the four refusal
    // messages.
    let settings = live_settings(app);
    let consumer = match audit_admit(
        latches(),
        &body,
        &state.root(),
        |c| state.consumer_exposed(c),
        |agent, tab| latch_scope(app, &settings, agent, Some(tab)),
        |scope| GatePolicy::resolve(&settings, scope),
    ) {
        Ok(c) => c,
        Err(msg) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(msg),
            };
            // 200 with `ok:false`, like every other tool-level error on this
            // route: the child renders `error` as the tool result. Sent BEFORE
            // `write_ndjson_head`, so this is a plain single-JSON body — which
            // the child's line reader already handles (`parse_result_line` over
            // the unterminated trailing line). A refusal written after the
            // ndjson head would corrupt the stream, so every refusal this route
            // can take is funnelled through this one arm.
            return write_json(stream, 200, &r).await;
        }
    };

    write_ndjson_head(stream, "audit").await?;

    // Run the scan concurrently with the heartbeat interval: whichever branch
    // fires, `run_audit` still owns clearing `scanning`.
    let run_fut = crate::audit::mcp::run_audit(&state, category, consumer, body.tab.as_deref());
    tokio::pin!(run_fut);

    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await; // consume the immediate first tick

    let result = loop {
        tokio::select! {
            biased;
            r = &mut run_fut => break r,
            _ = beat.tick() => {
                // A failed heartbeat write means the caller went away. Stop
                // beating, but drain the (uncancellable) scan to completion so
                // the runner leaves `scanning` — then drop the unsendable result.
                if stream.write_all(HEARTBEAT_LINE).await.is_err() {
                    debug!("audit loopback: heartbeat write failed; caller gone, draining scan");
                    let _ = (&mut run_fut).await;
                    return Ok(());
                }
                stream.flush().await.ok();
            }
        }
    };

    let r = match result {
        // #48 M-6: the report crosses the delivery boundary here — screened,
        // enveloped under the scanner preamble, headered if a layer fired. It is
        // a `RawReport`, not a `String`, so this call cannot be dropped by
        // omission (see `audit::mcp::RawReport`).
        //
        // Settings are re-read rather than reusing the `settings` snapshot
        // `audit_admit` gated on: a scan can run for minutes, and the envelope is
        // resolved **once per delivery** for exactly the reason
        // `spotlight::recall_envelope` is — the posture that applies is the one
        // in force when the text enters the conversation, not the one in force
        // when the scan was admitted.
        Ok(report) => RunResult {
            ok: true,
            text: Some(
                report
                    .deliver(crate::audit::mcp::Delivery {
                        settings: &live_settings(app),
                        scope: crate::settings::injection::Scope::for_tab(
                            crate::graph::source_for_consumer(consumer),
                            body.tab.as_deref(),
                        ),
                    })
                    .await,
            ),
            error: None,
        },
        // Busy / disabled / no-tools errors intentionally arrive here as
        // `ok:false` — a tool result the child surfaces, not a protocol failure.
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(e),
        },
    };
    write_result_line(stream, &r, "audit").await
}

/// A `POST /context/retrieve` request body (from the Claude UserPromptSubmit
/// hook or the OpenCode injection plugin).
#[derive(Deserialize)]
struct ContextRetrieveBody {
    /// The calling session's working directory; the project root is resolved
    /// from it (defaults to `.`).
    #[serde(default)]
    cwd: Option<String>,
    /// The user's prompt to rank context against.
    prompt: String,
    /// The agent session id (scopes the working-set boost); optional.
    #[serde(default)]
    session_id: Option<String>,
    /// V13 Phase C: which agent shim is calling — `"claude"` (set by
    /// `context_hook.rs`) or `"opencode"` (set by the generated plugin);
    /// absent/`None` for an unrecognized caller. Recorded on the checkpoint
    /// it triggers (see [`WorkbenchService::on_prompt`](crate::workbench::WorkbenchService::on_prompt)),
    /// not otherwise used by context retrieval itself.
    #[serde(default)]
    agent: Option<String>,
    /// V33: the cImp TAB this prompt belongs to — `--tab <id>` baked into the
    /// `--context-hook` command at spawn (`tabs::config`), or `CIMP_TAB_ID`
    /// from the generated OpenCode plugin. Recorded on the checkpoint this
    /// prompt triggers so the Timeline can tell two same-agent tabs on one
    /// project root apart; nothing about context retrieval reads it.
    ///
    /// `#[serde(default)]` because a hook shim from an older build sends no
    /// such field, and a prompt must never fail for lack of identity — the
    /// checkpoint is simply written without a tab, exactly as before.
    #[serde(default)]
    tab: Option<String>,
}

/// V33: the conversation identity recorded on the prompt-tap checkpoint this
/// route fires — the join key between a Timeline row and a
/// `Screen::Contamination` activity row.
///
/// **The tab id goes through [`tab_identity`]**, the same #45 narrowing every
/// other identity-taking route uses, so only a *configured AI tab id* is ever
/// recorded. A `tab` naming no configured tab is a forged or stale claim, and
/// writing it into a checkpoint would put a fabricated attribution on a record
/// whose whole purpose is to be trusted after an incident. It degrades to no
/// tab — which reads as "cannot attribute this checkpoint", not as "some other
/// tab". `Anonymous` (a hook shim from a build before `--tab` was baked in, or
/// an OpenCode plugin file not yet regenerated) lands in the same place, which
/// is exactly the pre-V33 row.
///
/// `session_id` and `agent` are recorded as sent. They are equally
/// caller-asserted, but neither can widen anything: they are compared for
/// equality against a contamination row and nothing else, and the framing
/// hazard they carry is handled where it lives — at the commit-trailer write
/// boundary (`workbench::shadow`'s `trailer_identity`).
///
/// Split out of the handler so the narrowing is exercised by a test rather than
/// re-implemented in one: a test that owned its own copy of this mapping would
/// stay green if the handler stopped calling it.
fn checkpoint_origin(
    settings: &crate::settings::Settings,
    body: &ContextRetrieveBody,
) -> crate::workbench::shadow::Origin {
    checkpoint_identity(
        settings,
        body.agent.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    )
}

/// The narrowing itself, shared by the prompt-tap trigger above and V33 Phase
/// F's pre-tool trigger ([`handle_tool_checkpoint`]) — ONE spelling, so the two
/// checkpoint writers cannot come to disagree about which `tab` claims are
/// believed.
fn checkpoint_identity(
    settings: &crate::settings::Settings,
    agent: Option<&str>,
    session_id: Option<&str>,
    tab: Option<&str>,
) -> crate::workbench::shadow::Origin {
    // V33 C5: the id is checked against the tabs of the consumer the body
    // asserts, normalised through the same `hook_agent` funnel the gated hook
    // routes use — a `tab` that names another harness's tab is a forged or
    // stale claim exactly as an invented one is, and lands in the same place.
    let tab = match tab_identity(settings, hook_agent(agent), tab) {
        TabIdentity::Configured(tab) => Some(tab.to_string()),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => None,
    };
    crate::workbench::shadow::Origin::new(
        agent.map(str::to_string),
        session_id.map(str::to_string),
        tab,
    )
}

/// `POST /context/retrieve`: rank files for the prompt and return the injectable
/// digest as `{ ok, text }`. Gated on `context_injection` — returns empty text
/// (never blocks a turn) when injection is off or nothing clears the threshold.
async fn handle_context_retrieve(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextRetrieveBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let cwd = body
        .cwd
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // V13 Phase C: fire the prompt-tap checkpoint trigger for EVERY prompt
    // that reaches this route, BEFORE the `context_injection` gate below —
    // checkpointing must fire even when injection is off or yields nothing
    // (Decision 4: decoupled from the injection toggle, reusing its
    // transport). Fire-and-forget: `on_prompt`'s own min-gap check is cheap
    // and the real snapshot work runs on a background task inside it, so
    // this never delays the turn waiting on a `git` round trip.
    // FIX 8 (V13 code review): only spawn the task at all when checkpoints
    // are actually on — `on_prompt`/`maybe_snapshot` already no-op when
    // they're off, but that check used to happen AFTER a task was already
    // spawned for every single prompt, which is needless per-prompt work
    // (a task spawn plus a settings read) for a feature the user has
    // disabled.
    if let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() {
        let workbench = workbench.inner().clone();
        if workbench.checkpoints_enabled() {
            let root = cwd.clone();
            // V33: the identity the Timeline is joined on. The settings read
            // sits INSIDE the `checkpoints_enabled` gate for FIX 8's reason —
            // a user with checkpoints off pays nothing for this.
            let origin = checkpoint_origin(&live_settings(app), &body);
            let prompt_head: String = body.prompt.chars().take(80).collect();
            tauri::async_runtime::spawn(async move {
                workbench.on_prompt(&root, origin, &prompt_head).await;
            });
        }
    }

    let empty = serde_json::json!({ "ok": true, "text": "", "files": [], "tokens_est": 0 });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    // The injection toggle is enforced here (the service's retrieve does not) so
    // the preview surface can reuse the same core while injection is off.
    if !graph.context_injection_enabled() {
        return write_json(stream, 200, &empty).await;
    }
    let r = graph.retrieve_context(&cwd, &body.prompt, body.session_id.as_deref());
    // V11 Phase B: prepend the once-per-session project map. Done here (the real
    // injection path), not in `retrieve_context`, so the preview surface — which
    // also calls `retrieve_context` — never consumes the once-per-session flag.
    let mut text = r.context_md;
    if let Some(map) = graph.session_greeting(&cwd, body.session_id.as_deref()) {
        text = if text.is_empty() {
            map
        } else {
            format!("{map}\n\n{text}")
        };
    }
    // V12 Phase F: drain any auto-check block a slow post-edit run parked for
    // this session (see `GraphService::post_edit`'s budget/park path) — a
    // turn is never blocked waiting for a check, but its result still reaches
    // the model on the very next opportunity.
    if let Some(pending) = graph.drain_auto_check(body.session_id.as_deref()) {
        text = if text.is_empty() {
            pending
        } else {
            format!("{text}\n\n{pending}")
        };
    }
    // Same char→token estimate as the retrieval core (shared divisor so the two
    // can't drift). Estimated from the FULL injected text (digest + greeting +
    // drained auto-check), not just the digest.
    let tokens_est = crate::graph::est_tokens(text.chars().count());
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": text, "files": r.files_used, "tokens_est": tokens_est }),
    )
    .await
}

/// A `POST /workbench/tool_checkpoint` request body — V33 Phase F's two
/// out-of-process fire seams: the Claude `PreToolUse` shim
/// (`crate::checkpoint_beacon`) and the OpenCode `tool.execute.before` plugin
/// hook. The worker seam does NOT come through here; it calls
/// `WorkbenchService::on_tool` directly (`offload::tools::dispatch`).
#[derive(Deserialize)]
struct ToolCheckpointBody {
    /// The calling session's working directory — the project root the shadow
    /// repo lives under. Defaults to `.` like every other hook route.
    #[serde(default)]
    cwd: Option<String>,
    /// Which harness is calling: `"claude"` / `"opencode"`. Normalised through
    /// [`hook_agent`], and it selects WHICH tool vocabulary the name below is
    /// checked against — the two namespaces are disjoint and must not be
    /// crossed.
    #[serde(default)]
    agent: Option<String>,
    /// The cImp TAB, baked into the hook command / the plugin file at spawn.
    /// Narrowed through [`checkpoint_identity`]; an unrecognised id degrades to
    /// "no tab", never to another tab.
    #[serde(default)]
    tab: Option<String>,
    /// The harness's own session id, recorded as sent.
    #[serde(default)]
    session_id: Option<String>,
    /// The tool about to run, in the CALLER's vocabulary (`Bash`, `edit`).
    /// Required — a checkpoint with no tool name is not a Phase F checkpoint.
    #[serde(default)]
    tool: Option<String>,
}

/// **The pre-tool checkpoint budget** (2026-08-13 amendment, locked): how long
/// [`handle_tool_checkpoint`] lets a snapshot run before it abandons it
/// unwritten.
///
/// Deliberately **below** the ~2 s both out-of-process callers wait — the Claude
/// shim's [`checkpoint_beacon::REPLY_TIMEOUT`](crate::checkpoint_beacon) and the
/// OpenCode plugin's `AbortSignal.timeout(2000)`. The ordering is the whole
/// point: the harness starts the tool the instant its hook stops waiting, so if
/// the caller's timer fired first the app would still be staging *into* the tool
/// call while believing it had a valid pre-tool checkpoint. Keeping the app's
/// budget under the caller's makes the app's own answer the one that decides,
/// and leaves ~200 ms for the reply to be written and read.
///
/// Not a per-call latency budget for the *user*: the throttle means most calls
/// never reach a snapshot at all, and this bound is only reached by a
/// `git add -A` over a work tree big enough to take seconds.
/// `pub(crate)` so `checkpoint_beacon`'s
/// `the_shim_waits_longer_than_the_app_takes_to_give_up` can assert the ordering
/// against the shim's own timeout. The two constants live in different files and
/// nothing else keeps them in the right order.
pub(crate) const TOOL_CHECKPOINT_BUDGET: Duration = Duration::from_millis(1800);

/// V33 Phase F: does `tool` change files on disk, **in `harness`'s own tool
/// vocabulary**?
///
/// Split out of [`handle_tool_checkpoint`] so the namespace selection is
/// exercised by a test rather than re-implemented in one — a test that owned its
/// own copy of this `match` would stay green after the handler stopped calling
/// it. `harness` has already been normalised through [`hook_agent`], so it is
/// one of the two-word vocabulary and the `_` arm is Claude's.
fn tool_checkpoint_is_mutating(harness: &str, tool: &str) -> bool {
    match harness {
        "opencode" => crate::offload::toolclass::opencode_native_mutates_fs(tool),
        _ => crate::offload::toolclass::mutates_fs(tool),
    }
}

/// `POST /workbench/tool_checkpoint` (V33 Phase F): take a Workbench checkpoint
/// **immediately before** a filesystem-mutating tool call, attributed to that
/// exact call.
///
/// # Why the tool name is re-checked here
///
/// Both callers pre-filter — Claude's hook is installed with an
/// `Edit|Write|MultiEdit|Bash` matcher, the plugin consults a baked
/// `CIMP_MUTATING_TOOLS` set — but neither is the authority. This route resolves
/// the name against `toolclass`'s reviewed tables, per harness:
/// [`mutates_fs`](crate::offload::toolclass::mutates_fs) for Claude's
/// capitalized vocabulary,
/// [`opencode_native_mutates_fs`](crate::offload::toolclass::opencode_native_mutates_fs)
/// for OpenCode's. A drifted matcher, a shim from a newer build, or a forged
/// POST from any local process therefore cannot mint a checkpoint for a tool
/// cImp does not classify as mutating.
///
/// **Crossing the two vocabularies would be silent, not loud**: `mutates_fs`
/// answers `false` for `edit` (an unknown name in cImp's own table), so reading
/// OpenCode's ids through it would disable the whole OpenCode seam while every
/// test that only exercised Claude stayed green.
///
/// # Containment posture
///
/// Behind the same bearer token as every other route, and **it takes no taint
/// gate** — deliberately. A checkpoint is a `git add -A` + `commit-tree` into
/// cImp's OWN shadow repo; it returns no project data to the caller (the reply
/// is `{ok, checkpointed}`, two booleans) and grants no capability, so there is
/// nothing for a latch to refuse. The abuse case for a forged POST is a spurious
/// snapshot, which the per-`(root, tab)` min-gap throttle and the tree-sha dedup
/// both bound — and which, unlike a refusal, costs the user nothing they wanted.
/// Gating it would instead mean that a tab which had touched external content
/// stopped getting checkpoints exactly when they matter most.
///
/// # The pre-tool budget (2026-08-13 amendment, locked)
///
/// **Both callers of this route stop waiting after ~2 s** — the Claude shim on
/// its reply-read timeout, the OpenCode plugin on its `AbortSignal.timeout(2000)`
/// — and the harness runs the tool the moment they do. A snapshot still staging
/// past that point is racing the very edit it exists to precede, so this route
/// hands [`WorkbenchService::on_tool`](crate::workbench::WorkbenchService::on_tool)
/// a deadline of [`TOOL_CHECKPOINT_BUDGET`] and the snapshot writes **nothing**
/// once it is spent. The alternative — let the caller give up and let the app
/// commit the row anyway — is the failure this amendment exists to close: a
/// checkpoint that sometimes contains the change it claims to predate silently
/// misleads a restore, which is strictly worse than having none.
///
/// The budget is deliberately *under* the callers' 2 s so the app's own answer
/// is what decides, rather than whichever timer happens to fire first.
///
/// The worker seam does not come through here and gets no deadline: it is
/// in-process and waits as long as the snapshot takes.
///
/// # The reply, and what it is not
///
/// `{ "ok": true, "checkpointed": <bool> }`. `checkpointed` deliberately does
/// not return a checkpoint id, because on a dedup hit the id would name another
/// trigger's (possibly another tab's) checkpoint and no caller may claim it (see
/// [`shadow::SnapshotOutcome`](crate::workbench::shadow::SnapshotOutcome)).
///
/// It means *"the trigger settled — nothing about this call is unaccounted
/// for"*: true for a checkpoint created, for a dedup hit, and for a throttled
/// call (whose tab already has a checkpoint newer than `checkpoint_min_gap_s`).
/// **False now also covers the one new case: the snapshot was abandoned against
/// the budget above**, i.e. exactly when no checkpoint can be said to precede
/// this call. Neither caller gates anything on it — the Claude shim reads it
/// only to make its wait mean something, the OpenCode plugin awaits it for the
/// ordering — so the user-facing report of a miss is the Activity row
/// `workbench` / `checkpoint_missed`, not this boolean.
async fn handle_tool_checkpoint(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ToolCheckpointBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let Some(tool) = body
        .tool
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return write_json(
            stream,
            400,
            &serde_json::json!({ "ok": false, "error": "missing `tool`" }),
        )
        .await;
    };
    let harness = hook_agent(body.agent.as_deref());
    if !tool_checkpoint_is_mutating(harness, tool) {
        // 200, not 400: a caller whose matcher is wider than the table is not
        // malformed, it is over-reporting, and a fail-open sensor must never
        // learn to treat that as an error. `checkpointed: false` is the whole
        // answer. No log line either — this is reachable once per non-mutating
        // matched call and would be unbounded chatter.
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "checkpointed": false }),
        )
        .await;
    }

    let Some(workbench) = app.try_state::<Arc<crate::workbench::WorkbenchService>>() else {
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "checkpointed": false }),
        )
        .await;
    };
    let workbench = workbench.inner().clone();
    if !workbench.checkpoints_enabled() {
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "checkpointed": false }),
        )
        .await;
    }
    let root = body
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let origin = checkpoint_identity(
        &live_settings(app),
        body.agent.as_deref(),
        body.session_id.as_deref(),
        body.tab.as_deref(),
    );
    // `harness:tool_name` — the locked value format. `bounded_id` caps the
    // caller-supplied half before it reaches a commit trailer; `trailer_identity`
    // rejects the framing hazards at the write boundary, and an over-long value
    // there would be dropped WHOLE, losing the harness prefix too.
    let source = format!("{harness}:{}", bounded_id(tool));
    // AWAITED, unlike the prompt-tap trigger's fire-and-forget spawn: the point
    // of this trigger is that the snapshot precedes the mutation, and BOTH
    // callers await this POST (the OpenCode plugin inside
    // `tool.execute.before`, the Claude shim since the 2026-08-13 amendment).
    // The wait is bounded twice over — by the throttle, which admits at most one
    // snapshot per `checkpoint_min_gap_s` per `(root, tab)` and is what makes
    // the common case free, and by the budget below, which is what stops a slow
    // one from outliving the caller's patience and minting a row for a tool call
    // that has already run.
    let checkpointed = workbench
        .on_tool(
            &root,
            origin,
            &source,
            Some(Instant::now() + TOOL_CHECKPOINT_BUDGET),
        )
        .await;
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "checkpointed": checkpointed }),
    )
    .await
}

// ── #48, finding M-7: the three `/context/*` hook routes' taint gate ───────
//
// The finding: `/context/post_edit`, `/context/should_read` and
// `/context/compaction` reached local capability with no `latches()` call of
// any kind and no `tab` in their bodies. `post_edit` in particular EXECUTES the
// project's configured check commands. "Only our own shim calls this route" is
// a convention, not a security property: the listener is reachable by any
// process running as this user, and the bearer token is in a discovery file
// that same process can read.
//
// The fix has two halves, and both were needed — a gate with no identity to
// resolve would have been ceremony:
//
// 1. **Identity now rides the wire.** `--tab <id>` is baked into all three hook
//    commands at spawn (`tabs::config::build_pre_args`, the same treatment
//    `--context-hook` and `--taint-beacon` already had), the shims forward it,
//    and the generated OpenCode plugin sends `CIMP_TAB_ID` on its `post_edit`
//    POST.
// 2. **The gate**, below — `/graph_run`'s shape, on [`LatchRoute::Hook`].
//
// **The residual, stated rather than papered over:** a body with no usable
// `tab` still resolves to no scope and is ADMITTED. That is the locked
// fail-open posture every tool-serving route here takes (`latch_scope`), and
// tightening it is the open decision F-5/H-8 tracks, not something to settle
// route-by-route: a shim from a build before this commit sends no `tab`, and
// failing closed would silently disable the read advisor and auto-check for
// every such tab. So a forged POST that simply omits `tab` is ungated here,
// exactly as it is on `/graph_run` and `/mcp/call`.

/// The class-table name `POST /context/post_edit` gates under. See its
/// [`toolclass::TABLE`] row for why it is LOCAL-CAPABILITY.
const HOOK_TOOL_POST_EDIT: &str = "hook_post_edit";
/// The class-table name `POST /context/should_read` gates under.
const HOOK_TOOL_SHOULD_READ: &str = "hook_should_read";
/// The class-table name `POST /context/compaction` gates under. TRUSTED, so
/// this gate admits every call today — see the row.
const HOOK_TOOL_COMPACTION: &str = "hook_compaction";

/// The taint decision the three `/context/*` hook routes take before they reach
/// [`crate::graph::GraphService`], as one function taking its dependencies as
/// arguments — the [`audit_admit`] shape, for the same two reasons: a handler
/// cannot reach capability without passing through it, and the decision is
/// testable without a `TcpStream` or an `AppHandle` (this crate has no
/// `tauri::test` mock — see [`latch_state_reply`]).
///
/// `Err(refusal)` means *this conversation may not have this*. Every caller
/// answers it with the route's own fail-safe reply — empty text, or a `pass`
/// verdict — and never with the refusal string: these are hooks, and a hook
/// that returns an error perturbs the turn it was supposed to be invisible to.
/// The refusal is not silent even so: [`LatchRegistry::gate`] writes the
/// [`Screen::LatchRefusal`](outbound::Screen) row (once per scope) that gives
/// it a user-visible consumer.
///
/// `agent` is caller-asserted, exactly as `consumer` is on `/graph_run`. It
/// selects which agent's key the scope is built under and nothing else; F-4
/// (`(consumer, tab)` is a verified pair on no route) is unchanged here, not
/// worked around.
fn hook_admit(
    reg: &LatchRegistry,
    tool: &'static str,
    agent: &'static str,
    tab: Option<&str>,
    scope_of: impl FnOnce(&'static str, Option<&str>) -> LatchScoping,
    policy_of: impl FnOnce(Option<&LatchScope>) -> GatePolicy,
) -> Result<(), &'static str> {
    let scoping = scope_of(agent, tab);
    let scope = scoping.scope();
    let policy = policy_of(scope);
    // `CallProvenance::http()`, not `internal()`: this is a POST from a local
    // process, and the launch token is readable by anything running as this
    // user, so it is never evidence that cImp itself decided the call (#45's
    // reasoning for the beacon route). It reaches no row today — provenance is
    // read only when an admitted call is EXTERNAL, and no name gated here is —
    // but stating it by omission is how the wrong origin gets inherited later.
    reg.gate(
        scope,
        LatchRoute::Hook,
        tool,
        policy,
        CallProvenance::http(),
    )
    .map(|_| ())
}

/// The agent key a hook body's caller-asserted `agent` resolves to. Absent ⇒
/// `claude`: `--precompact-hook` and `--read-hook` are installed only into
/// Claude's settings overlay, and a `post_edit` body with no `agent` is a shim
/// from a build before this field existed, which was a Claude shim.
fn hook_agent(agent: Option<&str>) -> &'static str {
    crate::graph::source_for_consumer(agent.unwrap_or("claude"))
}

/// A `POST /context/compaction` request body (the Claude `PreCompact` shim).
#[derive(Deserialize)]
struct ContextCompactionBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `"manual"` / `"auto"`; recorded, not currently branched on.
    #[serde(default)]
    #[allow(dead_code)]
    trigger: Option<String>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    /// `#[serde(default)]` because a shim from an older build sends none — see
    /// the residual note above.
    #[serde(default)]
    tab: Option<String>,
}

/// `POST /context/compaction` (V11 Phase D): always runs the session's
/// compaction side effects (clear injection dedup, mark post-compaction) and
/// returns a compact working-set/notes block as `{ ok, text }` to carry through
/// the summary. Never blocks — an empty block is returned as empty text.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_COMPACTION`], which
/// classifies TRUSTED — so this gate admits every call today, and the route is
/// inside the mechanism rather than beside it (demoting that one row is all it
/// takes to close the route, and its comment states what else a demotion must
/// do first). The block's content is why: paths, symbol NAMES and memory-note
/// text, no source text, with quarantined notes already excluded.
async fn handle_context_compaction(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: ContextCompactionBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_COMPACTION,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let block = graph.compaction_context(&cwd, body.session_id.as_deref());
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": block.unwrap_or_default() }),
    )
    .await
}

/// A `POST /context/should_read` request body (the Claude `PreToolUse` Read
/// advisor shim).
#[derive(Deserialize)]
struct ShouldReadBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    file_path: String,
    /// 1-based read offset, when the agent asked for a windowed read.
    #[serde(default)]
    offset: Option<u32>,
    /// V17 Phase B: the `Read` line limit, when the agent asked for a slice.
    /// Forwarded so the verdict can tell a full read from a head-peek (a
    /// deliberate slice always passes — Phase C's first-read branch).
    #[serde(default)]
    limit: Option<u32>,
    /// #48 (M-7): which shim is calling. See [`hook_agent`].
    #[serde(default)]
    agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves, baked into argv at spawn.
    #[serde(default)]
    tab: Option<String>,
}

/// `POST /context/should_read` (V11 Phase E): the read-advisor verdict for a
/// `Read`. Returns `{ ok, verdict: "pass" }` to let the read through, or
/// `{ ok, verdict: "remind", text }` to deny-with-content. Fails open to `pass`
/// on any missing state — the advisor must never block a legitimate read.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_SHOULD_READ`]
/// (LOCAL-CAPABILITY — the verdict hands back the file's outline, its symbol
/// body, or a unified diff of it, which is repo source text). **This does not
/// weaken the sentence above.** The gate's only reachable effect is to turn a
/// `remind` into a `pass`, because `pass` is the fail-safe every arm of this
/// route falls back to: a latched conversation gets its read through untouched
/// and pays only the tokens the advisor would have saved. The advisor can still
/// never block a legitimate read — after this change it can block strictly
/// fewer of them.
async fn handle_should_read(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let pass = serde_json::json!({ "ok": true, "verdict": "pass" });
    let body: ShouldReadBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_SHOULD_READ,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &pass).await;
    }
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &pass).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match graph.should_read(
        &cwd,
        body.session_id.as_deref(),
        &body.file_path,
        body.offset,
        body.limit,
    ) {
        Some(text) => {
            write_json(
                stream,
                200,
                &serde_json::json!({ "ok": true, "verdict": "remind", "text": text }),
            )
            .await
        }
        None => write_json(stream, 200, &pass).await,
    }
}

/// A `POST /activity/contract_drift` request body (V16 Feature 3): a hook
/// shim reporting a payload that was missing required fields.
#[derive(Deserialize)]
struct ContractDriftBody {
    shim: String,
    #[serde(default)]
    missing: Vec<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// The hook shims cImp itself installs — and, with [`DRIFT_SHIM_UNKNOWN`], the
/// **only** keys [`CONTRACT_DRIFT_SEEN`] can hold (#48 F-37).
///
/// Every entry is a literal one of this crate's own shim binaries sends:
/// `context_hook::report_contract_drift`'s first argument at four call sites,
/// plus the two beacons' hand-rolled bodies (they post through their own
/// dispatchers rather than through that helper). Spelling is pinned against
/// those sources by `tests::the_drift_shim_list_is_spelled_the_way_the_shims_spell_it`.
///
/// Drift here fails SAFE in both directions: an unlisted shim shares the
/// sentinel bucket (fewer rows, never more), and a listed name no shim sends is
/// a bucket nothing ever claims.
///
/// `checkpoint_beacon` was missing when V33 Phase F landed — it ships a shim
/// that reports drift under its own name, so its reports were folding into
/// [`DRIFT_SHIM_UNKNOWN`] alongside genuinely forged names and sharing that
/// bucket's doubling counter. Fail-safe, as documented, but it meant the newest
/// shim's payload drift was the one hardest to see.
const DRIFT_SHIMS: [&str; 6] = [
    "checkpoint_beacon",
    "compact_hook",
    "context_hook",
    "notify_hook",
    "read_hook",
    "taint_beacon",
];

/// The one bucket every shim name cImp does not ship shares. Parenthesized like
/// [`outbound::NO_TAB_IDENTITY`], so it cannot be confused with a real name.
const DRIFT_SHIM_UNKNOWN: &str = "(unrecognized shim)";

/// The ledger key for a caller-supplied `shim` string: its own entry in
/// [`DRIFT_SHIMS`], or the one shared sentinel.
///
/// Returns `&'static str` and not `String` **on purpose** — that is the bound
/// itself rather than a check that implements it. The ledger's key type makes a
/// caller-supplied string unable to become a key at all, so the key space is
/// `DRIFT_SHIMS.len() + 1` by construction and cannot drift back.
///
/// **Exact match, never a prefix** (after trimming): `"read_hook-forged"` is the
/// sentinel, not `read_hook`. A prefix or truncation rule here would let an
/// invented name claim a real shim's counter — [`bounded_id`]'s
/// ordering rule, one route over.
fn drift_shim_key(raw: &str) -> &'static str {
    let raw = raw.trim();
    DRIFT_SHIMS
        .into_iter()
        .find(|shim| *shim == raw)
        .unwrap_or(DRIFT_SHIM_UNKNOWN)
}

/// Rate-limit state for `handle_contract_drift`. A systematically broken payload
/// fires its shim on every hook invocation, and without a ledger one bad session
/// would flood the Activity store's 400-row graph ring.
///
/// **#48 F-37 / locked decision 42 — this used to be a `HashSet<(String,
/// String)>` keyed on the caller's own `shim` and `session_id`.** Both halves of
/// the key came off the wire, so any token-holder could grow it without limit and
/// evict the whole graph lane, taking genuine security rows out of a capped ring
/// with it. The fix is the bar `/activity/discovery_skipped` already meets
/// ([`DISCOVERY_REPORTS`]), in both of its halves:
///
/// * **The key is not the caller's** — it is [`drift_shim_key`]'s classification
///   of it, a `&'static str` from a compile-time list. Ten thousand invented shim
///   names buy **one** bucket.
/// * **Repeats cost `log2(n)` rows** — [`outbound::Doubling`], the same primitive
///   `claim_ssrf` and the discovery report use, and each row states how many
///   reports it stands for so a fold is never a silent drop.
///
/// **`session_id` is deliberately no longer part of the key.** It is
/// caller-supplied with nothing app-side to classify it against — this body
/// carries no tab, and the missing `session_id` is frequently the very drift
/// being reported — so keeping it would have left the unbounded half in place.
/// The cost is the documented "one row per shim per session" becoming "rows at
/// reports 1, 2, 4, 8 … per shim per app run": strictly more rows for a genuinely
/// broken shim, and the consumer is unaffected, since `drift.payload.v1` reads
/// events since process start and de-duplicates by shim
/// (`ipc::commands::advisor_signals`).
///
/// Process lifetime, unchanged and for the same reason as before.
static CONTRACT_DRIFT_SEEN: OnceLock<Mutex<HashMap<&'static str, outbound::Doubling>>> =
    OnceLock::new();

/// Count one drift report against the process ledger. See
/// [`CONTRACT_DRIFT_SEEN`].
fn claim_contract_drift(shim: &'static str) -> outbound::DoublingRow {
    let ledger = CONTRACT_DRIFT_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    drift_claim_in(&mut ledger, shim)
}

/// [`claim_contract_drift`] against a caller-owned ledger, so the key-space bound
/// and the doubling are assertable without process-global state (the suite runs
/// cases concurrently in one process). The twin of [`claim_in`].
fn drift_claim_in(
    ledger: &mut HashMap<&'static str, outbound::Doubling>,
    shim: &'static str,
) -> outbound::DoublingRow {
    ledger.entry(shim).or_default().claim()
}

/// How many field names one drift row may list. Every real payload check in this
/// crate has at most five (`read_hook::contract_checks`), so this is slack for a
/// future check rather than a limit anything genuine can reach.
const MAX_DRIFT_MISSING: usize = 12;

/// The caller's `missing` list, bounded in both dimensions before it reaches a
/// row (#48 F-37).
///
/// The list is an arbitrary count of arbitrary strings and it lands in the row's
/// `target` — which the store does **not** truncate (only `request` and
/// `response` are capped) and which `advisor_signals` copies verbatim into a
/// user-facing signal. Every genuine report is byte-identical to the plain
/// `join(", ")` this replaced; only abuse is cut, and the row says it was.
fn bounded_missing(raw: &[String]) -> String {
    let mut out: Vec<String> = raw
        .iter()
        .take(MAX_DRIFT_MISSING)
        .map(|f| bounded_id(f))
        .collect();
    if let Some(extra) = raw.len().checked_sub(MAX_DRIFT_MISSING).filter(|n| *n > 0) {
        out.push(format!("… (+{extra} more)"));
    }
    out.join(", ")
}

/// The activity row one drift report writes, or `None` when the ledger folds it
/// into an earlier one.
///
/// Split from the handler and given its ledger as a closure for the reason
/// [`record_discovery_skipped`] documents, plus one this route owns:
/// `activity::record_bg` has **no `cfg(test)` diversion**, so a row written
/// inside the handler is unobservable to the suite — which is why the pre-F-37
/// behaviour had no row-level test at all. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
///
/// Nothing here is left at the caller's length: [`bounded_id`] on the shim name
/// and on the session id, [`bounded_missing`] on the field list. The bounds are
/// applied **after** [`drift_shim_key`] has classified, so a truncated name
/// cannot claim a real shim's counter.
fn contract_drift_row(
    body: &ContractDriftBody,
    claim: impl FnOnce(&'static str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(drift_shim_key(&body.shim))
    else {
        return None;
    };
    let shim = bounded_id(&body.shim);
    let session = bounded_id(body.session_id.as_deref().unwrap_or_default());
    let missing = bounded_missing(&body.missing);
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the report is about the harness, not a project
            "harness".to_string(),
            "contract_drift".to_string(),
            format!("{shim}: {missing}"),
            missing.chars().count(),
            0,
            false, // a drift report is never "ok" — it flags the entry in the feed
            // The report is about the harness shim, not a tab's call — but
            // the session it drifted in is known and is the join key.
            //
            // #48 F-20 left this ALONE, and `Unattributed` is honest here:
            // `ContractDriftBody` carries no `tab` field at all, so this
            // writer genuinely does not know. The shim *does* (`--tab {tab}`
            // is baked into its hook command line, `tabs/config.rs`), so the
            // fix is a wire change — `#[serde(default)] tab: Option<String>`
            // on the body plus the shim sending it — and both skew directions
            // degrade safely. That is a shim/app contract change and belongs
            // with F-6's drift-canary work, not here.
            crate::activity::Attribution::Unattributed,
            Some(session.clone()),
        ),
        request: format!(
            "shim {shim} payload missing required fields (session {session}) — report {total} \
             from this shim this app run, {suppressed} folded into it"
        ),
        response: missing,
    })
}

/// `POST /activity/contract_drift` (V16 Feature 3): record a shim's
/// payload-drift report as an Activity event (`source: "harness"`,
/// `tool: "contract_drift"`), rate-limited per shim by [`CONTRACT_DRIFT_SEEN`].
/// Always answers `{ok: true}` — the shim is fail-open and fire-and-forget.
///
/// The 400 on a malformed body is this route's own long-standing contract and is
/// **not** the discipline `handle_discovery_skipped` follows (one constant reply
/// on every path, locked decision 37). The difference is deliberate: that route
/// exists so a *child* can report containment and must give a prober no oracle;
/// this one answers a shim of ours that is already misbehaving, and locked
/// decision 42 moved the bound, not the protocol.
async fn handle_contract_drift(stream: &mut TcpStream, req: &Request) -> AppResult<()> {
    let ok = serde_json::json!({ "ok": true });
    let body: ContractDriftBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    if let Some(record) = contract_drift_row(&body, claim_contract_drift) {
        crate::activity::record_bg(record);
    }
    write_json(stream, 200, &ok).await
}

// ── V35 Phase I: CHP — the protocol version, and the hello ──────────────────

/// Observe the `chp` version a routed POST carries, for the stale-artifact
/// report (`harness::chp`, milestone decision 9 / design D5).
///
/// Called once per request from the dispatcher, **beside** the route table
/// rather than inside the handlers. Three properties make that safe, and each is
/// the reason the phase's "zero behavior change" claim is a fact rather than a
/// hope:
///
/// * it **reads only** — no reply, no early return, no error path a route could
///   inherit. A malformed body here is ignored; the route's own handler still
///   owns its 400;
/// * the routes' body types are **untouched**, so every existing
///   deserialization test still pins the same shape;
/// * the tab id is validated against the user's configured AI tabs before
///   anything is stored, exactly as [`handle_latch_beacon`] validates its own —
///   so the peer registry's key space is `configured tabs × 2 agents` and not
///   "whatever a request body said" (#45's rule, one route surface over).
///
/// **Cost on the hot path.** Every prompt and every tool call passes here, so
/// the common case must not clone `Settings`. It does not: the second and every
/// later message from a tab hits [`crate::harness::chp::already_seen`], a single
/// map lookup, and returns. Only a *new* `(agent, tab, chp)` triple — i.e. a tab
/// launching, or a tab's artifact having changed — pays one settings read.
fn note_chp(app: &AppHandle, route: &str, body: &[u8]) {
    let Some((env, tab)) = crate::harness::chp::envelope(route, body) else {
        return;
    };
    let chp = env.chp.unwrap_or(crate::harness::chp::PRE_CHP);
    let agent = crate::graph::source_for_consumer(env.agent_token());
    if crate::harness::chp::already_seen(agent, &tab, chp) {
        return;
    }
    let settings = live_settings(app);
    if !is_configured_tab(&settings, agent, &tab) {
        return;
    }
    crate::harness::chp::note_push(agent, &tab, chp, crate::activity::now_ms());
}

/// A `POST /session/hello` body — V35 Phase I, design D3.
///
/// Every field is optional and every one is caller-supplied. The handler bounds
/// each before it can reach a row or the panel; nothing here is believed beyond
/// "this local process said so", which is the standard every `Origin::Http`
/// producer on this listener is held to.
#[derive(Deserialize)]
struct SessionHelloBody {
    /// The protocol version the artifact speaks. Absent ⇒ pre-CHP, never an
    /// error — a hello is exactly the message an old artifact would not send at
    /// all, so tolerating its absence here is belt-and-braces rather than a
    /// live path.
    #[serde(default)]
    chp: Option<u32>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` like
    /// every other route's discriminator.
    #[serde(default)]
    agent: Option<String>,
    /// The cImp tab this artifact was generated for. Required in practice: a
    /// hello with no tab has nothing to key, and one naming an unconfigured tab
    /// is refused (see the handler).
    #[serde(default)]
    tab: Option<String>,
    /// The harness's own version, when it exposes one to its extensions.
    #[serde(default)]
    harness_version: Option<String>,
    /// The CHP events this artifact will actually push, with its per-tab flags
    /// applied.
    #[serde(default)]
    serves: Vec<String>,
    /// …and the rest, each with a reason.
    #[serde(default)]
    cannot: Vec<SessionHelloUnable>,
}

/// One `cannot` entry: a capability this artifact will not serve, and why.
#[derive(Deserialize)]
struct SessionHelloUnable {
    #[serde(default)]
    id: String,
    #[serde(default)]
    why: String,
}

/// How many `serves` / `cannot` entries one hello may declare.
///
/// The live vocabulary is 17 ids (`harness::chp::EVENTS`), so this is slack for
/// a future event rather than a limit anything genuine can reach — the same
/// shape and the same reasoning as [`MAX_DRIFT_MISSING`]. Without it, `serves`
/// is an unbounded list of unbounded strings that reaches an in-memory registry
/// and a Settings panel.
const MAX_HELLO_DECLARATIONS: usize = 32;

/// The doubling ledger for hello rows, keyed on the **resolved** `agent:tab` —
/// which is only ever reached after [`is_configured_tab`] accepted it, so the
/// key space is bounded by the user's own tab list exactly as
/// [`DISCOVERY_REPORTS`]'s is.
///
/// Two gates, not one, and they catch different things: the row is written only
/// when the hello actually CHANGED what cImp knows (a plugin re-loading with the
/// same declaration is silent), and repeats of a genuinely flip-flopping
/// declaration cost `log2(n)` rows. Process lifetime, following its two
/// siblings.
static HELLO_SEEN: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one hello against the process ledger. See [`HELLO_SEEN`].
fn claim_hello(key: &str) -> outbound::DoublingRow {
    let ledger = HELLO_SEEN.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// The caller's declaration list, bounded in both dimensions before it reaches
/// the peer registry — the [`bounded_missing`] discipline applied to `serves`
/// and to `cannot`.
fn bounded_declarations(raw: &[String]) -> Vec<String> {
    raw.iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|s| bounded_id(s))
        .collect()
}

/// The Activity row one hello writes, or `None` when nothing changed.
///
/// Split from the handler for the reason [`contract_drift_row`] documents:
/// `activity::record_bg` has no `cfg(test)` diversion, so a row written inside a
/// handler is unobservable to the suite. Returning the record makes what a
/// caller can put in the store assertable without touching the global store.
fn hello_row(
    agent: &'static str,
    tab: &str,
    chp: u32,
    version: &str,
    serves: &[String],
    cannot: usize,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
) -> Option<crate::activity::ActivityRecord> {
    let outbound::DoublingRow::Write { total, suppressed } = claim(&format!("{agent}:{tab}")) else {
        return None;
    };
    let version = if version.is_empty() {
        "version not declared".to_string()
    } else {
        format!("v{version}")
    };
    let target = format!(
        "{agent}/{tab}: chp {chp} ({version}) — serves {}, cannot {cannot}",
        serves.len()
    );
    Some(crate::activity::ActivityRecord {
        entry: crate::activity::ActivityEntry::new(
            // The lane `contract_drift` already uses for harness-contract facts,
            // with the same `source: "harness"`. Deliberately NOT a new
            // retention lane: a hello fires once per tab launch, and the two
            // rows a reader wants side by side ("this plugin introduced itself"
            // / "this shim's payload broke") belong in one feed.
            crate::activity::ActivityKind::Graph,
            crate::activity::now_ms(),
            String::new(), // no root — the hello is about a harness, not a project
            "harness".to_string(),
            "chp_hello".to_string(),
            target,
            serves.len(),
            0,
            // A hello is a normal, healthy event — unlike a drift report, which
            // flags its entry.
            true,
            crate::activity::Attribution::Tab(tab.to_string()),
            None,
        ),
        request: format!(
            "serves: {}",
            if serves.is_empty() {
                "(nothing declared)".to_string()
            } else {
                serves.join(", ")
            }
        ),
        response: format!(
            "hello {total} from this tab this app run, {suppressed} folded into it"
        ),
    })
}

/// `POST /session/hello` (V35 Phase I, design D3): a generated harness artifact
/// introducing itself — the protocol version it speaks, the harness version it
/// runs under (when the harness exposes one), and what it will and will not
/// serve.
///
/// # Nothing gates on this, and that is the phase's exit criterion
///
/// `serves` / `cannot` are RECORDED and DISPLAYED, never consulted by a
/// capability. Negotiation becomes load-bearing in Phase L; making it so here
/// would be a behavior change dressed as a declaration — and would hand an
/// artifact the power to switch cImp features off by lying about itself.
///
/// **`serves` is not a trust claim in either direction.** An artifact declaring
/// `tool.gate` has said nothing cImp relies on: the gate's authority is cImp
/// computing the verdict at `/latch/state`, and the artifact's only power is to
/// refuse MORE than it was told to.
///
/// # Auth, and the same honesty clause every route here owes
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check. The launch
/// token is readable by any process running as this user, so "authenticated"
/// means *a local process*, never *cImp's own plugin*. Which is why the tab id
/// is validated ([`is_configured_tab`]) and every string is bounded before it
/// reaches the registry or the Settings panel.
///
/// # Answers
///
/// `200 {ok, chp}` — the ack carries the SERVER's version so a future client can
/// adapt to an older cImp. `400` on a malformed body or an unconfigured tab,
/// following [`handle_latch_beacon`]'s discipline rather than
/// [`handle_discovery_skipped`]'s constant-ack one: this route answers cImp's
/// own generated artifact, which is fail-open and discards the reply, and a
/// rejected tab is a fact worth a log line.
async fn handle_session_hello(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: SessionHelloBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let agent = crate::graph::source_for_consumer(body.agent.as_deref().unwrap_or("claude"));
    let tab = body.tab.as_deref().map(str::trim).unwrap_or("");
    let settings = live_settings(app);
    if tab.is_empty() || !is_configured_tab(&settings, agent, tab) {
        warn!(
            target: "offload",
            agent,
            tab = %bounded_id(tab),
            "loopback: /session/hello rejected — not a configured tab id"
        );
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(
                "/session/hello accepts configured AI tabs only — a hello with no tab has \
                 nothing to key"
                    .to_string(),
            ),
        };
        return write_json(stream, 400, &r).await;
    }
    let chp = body.chp.unwrap_or(crate::harness::chp::PRE_CHP);
    let version = bounded_id(body.harness_version.as_deref().unwrap_or("").trim());
    let serves = bounded_declarations(&body.serves);
    let cannot: Vec<crate::harness::chp::Unable> = body
        .cannot
        .iter()
        .take(MAX_HELLO_DECLARATIONS)
        .map(|u| crate::harness::chp::Unable {
            id: bounded_id(&u.id),
            why: bounded_id(&u.why),
        })
        .collect();
    let changed = crate::harness::chp::note_hello(
        agent,
        tab,
        chp,
        &version,
        serves.clone(),
        cannot.clone(),
        crate::activity::now_ms(),
    );
    if changed {
        if let Some(record) = hello_row(agent, tab, chp, &version, &serves, cannot.len(), claim_hello)
        {
            crate::activity::record_bg(record);
        }
    }
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "chp": crate::harness::chp::CHP_VERSION }),
    )
    .await
}

// ── #48 F-32 / locked decision 37: a child reports what containment did ──────

/// A `POST /activity/discovery_skipped` request body — a cImp stdio MCP child
/// saying it skipped one or more candidate discovery entries and reached this
/// instance anyway (#48 F-32).
///
/// Modelled field-for-field on [`LatchBeaconBody`]'s two identity fields,
/// including the `#[serde(default)] Option<String>` spelling and the
/// `source_for_consumer(…unwrap_or("claude"))` normalisation, so one tab is
/// named the same way from every route. **`consumer` is a BODY field**: the
/// query-string form exists only on `/mcp/call`, whose body is not ours — it is
/// MCP JSON-RPC, owned by another protocol — so cImp's transport metadata cannot
/// go in it. Every route whose body cImp defines end to end carries `consumer`
/// here.
///
/// **Three deliberate omissions, each closing an attack:**
///
/// * **No `cwd` and no path of any kind.** `/audit/run` needs the child's cwd
///   for its wrong-instance check; this does not. A body-supplied path would let
///   a caller file a *security* row under a project it is not about. The root is
///   derived app-side from the tab ([`tab_root_key`]) or left honestly empty.
/// * **No free-text field.** `/latch/beacon` needs [`bounded_tool`] purely
///   because it accepts a caller-chosen `tool` string. With no such field there
///   is no truncation and no control-sequence question at all: this row's `tool`
///   column is the fixed literal `"discovery"`.
/// * **No pid, port or root of the skipped entries.** They would be
///   attacker-chosen strings presented to an incident reader as forensic fact.
///   What the app can say about the directory, it observes itself
///   ([`discovery_census`]).
#[derive(Deserialize, Default)]
struct DiscoverySkippedBody {
    /// The cImp tab id the reporting child was spawned for. Absent ⇒
    /// [`crate::activity::Attribution::Unattributed`], never `Headless`.
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    consumer: Option<String>,
    /// How many candidates the child says it skipped. **Caller-asserted**, and
    /// the row says so — see [`bounded_skips`] for the bound and
    /// [`discovery_row`] for the honesty clause that states it.
    #[serde(default)]
    skipped: u32,
}

/// The one and only response body this route ever produces.
///
/// A constant rather than a serialized value so *"the response is not the
/// signal"* is a fact about the code and not a claim about it — see
/// [`handle_discovery_skipped`].
const DISCOVERY_ACK: &[u8] = br#"{"ok":true}"#;

/// The row's `tool` column: a fixed literal, because nothing in the body may
/// choose it.
const DISCOVERY_TOOL: &str = "discovery";

/// `POST /activity/discovery_skipped` (#48 F-32, locked decision 37): record
/// that a cImp MCP child skipped candidate discovery entries which did not
/// answer a token-authenticated `GET /health`, and reached this instance anyway.
///
/// **Why a new route rather than an extension of `/activity/contract_drift`.**
/// That route already is a token-authenticated child→app activity-row writer —
/// the finding's claim that none existed was wrong — and reusing it is still
/// wrong, for four reasons: it writes `ActivityKind::Graph`, whose single
/// 400-row lane is shared with every real graph tool call (a security row there
/// is evictable by ordinary work, and a flood there evicts ordinary work); its
/// body carries no `tab`, so its row is honestly `Unattributed` and naming a tab
/// would be the wire change anyway; `activity::record_bg` has **no `cfg(test)`
/// diversion**, which is exactly why F-20's owed test was never written, while
/// `outbound::record_flag` does; and its dedup ledger was keyed on two
/// caller-supplied strings with no bound (F-37, filed separately — since closed
/// by locked decision 42, which gave that ledger this one's discipline and its
/// row a `contract_drift_row` seam for the same reason. The first three reasons
/// are unaffected and this route still stands on its own).
///
/// # Auth
///
/// Bearer, inherited from the pre-dispatch [`authorized`] check — no route-level
/// auth code. And the honesty clause every `Origin::Http` producer owes: **the
/// launch token is readable by any process running as this user** (from
/// `.cimp-offload.json`, from `.cimp-discovery/<pid>.json`, and from the
/// generated OpenCode plugin inside the project tree). "Authenticated" here
/// means *a local process*, never *cImp's own child*.
///
/// # The response is not the signal
///
/// **`200` with the byte-identical body [`DISCOVERY_ACK`] on every single
/// path** — malformed JSON, an empty body, an unknown tab, an anonymous tab,
/// `skipped: 0`, `skipped: 9999`, a row written, a row suppressed by the gate.
/// This function has exactly one exit and nothing before it can return early.
///
/// That **diverges deliberately** from both siblings, and the divergence is the
/// point: `handle_latch_beacon` answers 400 to an unknown tab (a tab-id
/// enumeration oracle in the other direction, moot there because a token-holder
/// can read `settings.json` anyway, but not moot as a precedent) and
/// `handle_contract_drift` answers 400 to a parse error. Locked decision 37
/// requires this route to answer identically on every path. Follow the decision,
/// not the siblings — pinned by
/// `tests::the_discovery_report_answers_identically_on_every_path`.
///
/// The real signal has three consumers, none of them this reply: the activity
/// row (the user consumer F-32 exists to add), a `warn!` on `target: "offload"`
/// (the operator consumer), and the child's own unchanged `eprintln!`.
async fn handle_discovery_skipped(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    // Everything that can vary happens in here and returns `()`. No `?`, no
    // early return, no branch on the outcome.
    note_discovery_skipped(app, &req.body);
    write_simple(
        stream,
        200,
        "application/json; charset=utf-8",
        DISCOVERY_ACK,
    )
    .await
}

/// The app-side facts about a **configured** tab that a discovery row needs and
/// a request body must never be allowed to supply.
struct TabFacts {
    /// [`tab_root_key`] — resolved from settings, never from the wire.
    root: String,
    /// The V28 live-session registry's answer for this tab, never the wire's.
    session: Option<String>,
}

/// [`handle_discovery_skipped`] minus the socket: parse the body and record the
/// row, swallowing everything.
///
/// Split so the route's single exit is structural, and so the half that needs an
/// `AppHandle` (which this crate cannot mock) is one thin frame that injects the
/// two app-derived facts into [`record_discovery_skipped`] as a closure — the
/// same seam `mark_live_session_from_event` uses.
fn note_discovery_skipped(app: &AppHandle, raw: &[u8]) {
    // A parse failure is NOT an error path here: it degrades to a default body,
    // whose `skipped: 0` writes no row. Answering 400 would have been a second
    // response shape, i.e. the oracle this route exists without.
    let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
    // ONE settings read for the whole request, the discipline `/mcp/call`
    // documents: the tab-identity check and the root resolution must not run
    // against two snapshots.
    let settings = live_settings(app);
    record_discovery_skipped(
        &settings,
        &body,
        claim_discovery_report,
        |tab, agent| TabFacts {
            root: tab_root_key(app, &settings, tab),
            session: app
                .try_state::<Arc<crate::graph::GraphService>>()
                .and_then(|g| g.live_session_for_tab(tab, agent)),
        },
    );
}

/// The `skipped` count a row may state, decided at the parse boundary.
///
/// * `None` ⇒ **no row at all.** A report of zero skips is not a report — a
///   genuine child returns before posting — and it is also what a malformed or
///   empty body degrades to. So "a caller can make the store write a row" costs
///   it at least a well-formed claim.
/// * `Some((n, clamped))` ⇒ write a row for `n`, and say so if it was clamped.
///
/// The ceiling is [`MAX_DISCOVERY_PROBES`], and it is not a guess: a single
/// resolution cannot skip more than its probe budget — [`Probe::answers`]
/// enforces that — so any larger value is *definitionally* not something a
/// genuine child produced. It is clamped rather than rejected because rejecting
/// would need a second response shape, which is the oracle
/// [`handle_discovery_skipped`] exists without.
fn bounded_skips(raw: u32) -> Option<(u32, bool)> {
    if raw == 0 {
        return None;
    }
    let cap = MAX_DISCOVERY_PROBES as u32;
    Some((raw.min(cap), raw > cap))
}

/// The per-key doubling ledger for discovery reports (#48 F-32).
///
/// **The key space is bounded by something the caller does not control**, which
/// is the property [`CONTRACT_DRIFT_SEEN`] lacked until decision 42 gave it one
/// too (F-37 — its key is a `&'static str` from a fixed list, a stricter bound
/// than this one because that route has no tab list to key on): entries are keyed on
/// the *resolved scope label*, so a configured tab gets its own counter and
/// `Anonymous` + `Unknown(_)` share **one** sentinel bucket per consumer. A
/// caller inventing ten thousand tab ids therefore gets one counter and
/// `log2`-many rows, not ten thousand of each. Map size is bounded by
/// `2 × (configured AI tabs + 1)`.
///
/// Process lifetime, following `CONTRACT_DRIFT_SEEN`'s precedent, and that is a
/// decision rather than an omission: the doubling makes process lifetime cheap,
/// and — unlike a latch or a budget — this is not a per-conversation
/// entitlement, so there is nothing a session rotation should restore.
static DISCOVERY_REPORTS: OnceLock<Mutex<HashMap<String, outbound::Doubling>>> = OnceLock::new();

/// Count one report against the process ledger. See [`DISCOVERY_REPORTS`].
fn claim_discovery_report(key: &str) -> outbound::DoublingRow {
    let ledger = DISCOVERY_REPORTS.get_or_init(Default::default);
    let mut ledger = ledger.lock().unwrap_or_else(PoisonError::into_inner);
    claim_in(&mut ledger, key)
}

/// [`claim_discovery_report`] against a caller-owned ledger, so the key-space
/// bound and the doubling are assertable without process-global state (the
/// suite runs cases concurrently in one process).
fn claim_in(ledger: &mut HashMap<String, outbound::Doubling>, key: &str) -> outbound::DoublingRow {
    ledger.entry(key.to_string()).or_default().claim()
}

/// What the APP itself currently sees in `.cimp-discovery/`.
///
/// The half of the row a request cannot forge: the app runs on the same machine
/// as the child, so instead of believing a claim about the directory it lists
/// it. Called **only on the write path** (after the doubling gate), so it can
/// never become a filesystem-scan amplifier under a flood.
struct DirCensus {
    /// Parseable per-instance entries present right now.
    entries: usize,
    /// …of which do not belong to this process.
    foreign: usize,
}

fn discovery_census() -> DirCensus {
    let own = std::process::id();
    let all = read_all_discoveries();
    DirCensus {
        entries: all.len(),
        foreign: all.iter().filter(|d| d.pid != own).count(),
    }
}

/// Everything one discovery row states, gathered so [`discovery_row`] can stay
/// pure.
struct DiscoveryReport {
    /// The clamped, caller-asserted skip count.
    skipped: u32,
    /// Whether the caller's number exceeded the probe budget.
    clamped: bool,
    /// Reports this scope has filed (the doubling ledger's `total`).
    total: u32,
    /// How many reports this row stands for beyond itself.
    suppressed: u32,
    /// What the app observed for itself.
    observed: DirCensus,
}

/// Record one discovery report, given a settings snapshot and a way to resolve
/// a configured tab's app-side facts.
///
/// This is where locked decision 37's bar is enforced, clause by clause. A
/// token-holder can cause a row, and cannot:
///
/// * **name a non-configured tab** — the id is re-classified through
///   [`tab_identity`] against the user's own tab list. `Configured` ⇒
///   `Attribution::Tab`; `Unknown` ⇒ `Unrecognized` (bounded, [`bounded_id`],
///   because the id is caller-chosen and unbounded on the wire); `Anonymous` ⇒
///   **`Unattributed`, never `Headless`**. `Headless` is a *positive* claim —
///   "a worker run with no tab behind it" — and a body-supplied tab is
///   indistinguishable from an invented one, so claiming it would be F-20's
///   defect and F-29's, one producer further on. `Attribution::from_child_argv`
///   is forbidden here by its own doc for exactly that reason.
/// * **say anything a genuine row could not** — every remaining field is
///   app-derived: `root` from [`tab_root_key`], `session` from the V28 live
///   registry, `consumer` normalized to one of two words, `tool` a fixed
///   literal, `origin` fixed to `Http`, `skipped` clamped by [`bounded_skips`],
///   and the directory census observed rather than asserted.
/// * **cost another lane a row** — `Screen::DiscoverySkipped` is its own H-9
///   retention lane, so a flood here evicts only discovery rows
///   (`activity::tests::no_screen_can_evict_another_screens_rows` covers it for
///   every screen, this one included, without an edit).
/// * **exceed `log2(n)` rows in its own lane** — [`DISCOVERY_REPORTS`].
/// * **touch any latch** — nothing in this path reaches `latches()`; it holds no
///   registry handle and creates no entry, which
///   `every_loopback_route_declares_what_it_does_about_the_latch` checks against
///   the handler's source rather than believing.
///
/// `claim` and `facts` are injected for the same reason and it is not only
/// testability: the ledger is a process-global map and the facts need an
/// `AppHandle` this crate cannot mock, so a test that had to go through either
/// would be racing its neighbours or unable to run at all. Production wires
/// [`claim_discovery_report`] here — pinned by
/// `tests::the_discovery_report_never_reaches_the_hook_shims_path`, which reads
/// the wiring out of the source rather than trusting it.
fn record_discovery_skipped(
    settings: &crate::settings::Settings,
    body: &DiscoverySkippedBody,
    claim: impl FnOnce(&str) -> outbound::DoublingRow,
    facts: impl FnOnce(&str, &'static str) -> TabFacts,
) {
    let Some((skipped, clamped)) = bounded_skips(body.skipped) else {
        return;
    };
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or("claude"));
    let identity = tab_identity(settings, agent, body.tab.as_deref());
    // The scope label doubles as the flood key, which is deliberate: both want
    // "the identity this call actually resolved to", and the identity-less cases
    // must collapse onto one bucket rather than onto whatever the caller typed.
    let scope = match identity {
        TabIdentity::Configured(tab) => format!("{agent}:{tab}"),
        TabIdentity::Anonymous | TabIdentity::Unknown(_) => {
            format!("{agent}:{}", outbound::NO_TAB_IDENTITY)
        }
    };
    let outbound::DoublingRow::Write { total, suppressed } = claim(&scope) else {
        return;
    };

    let (attribution, root, session) = match identity {
        TabIdentity::Configured(tab) => {
            let f = facts(tab, agent);
            (
                crate::activity::Attribution::Tab(tab.to_string()),
                f.root,
                f.session,
            )
        }
        TabIdentity::Unknown(tab) => (
            crate::activity::Attribution::Unrecognized(bounded_id(tab)),
            String::new(),
            None,
        ),
        TabIdentity::Anonymous => (
            crate::activity::Attribution::Unattributed,
            String::new(),
            None,
        ),
    };

    let row = discovery_row(
        outbound::Origin::Http,
        &DiscoveryReport {
            skipped,
            clamped,
            total,
            suppressed,
            observed: discovery_census(),
        },
    );
    // The operator consumer. The user consumer is the row below; the child's own
    // stderr line is the third and is unchanged.
    warn!(
        target: "offload",
        agent,
        scope = %scope,
        skipped,
        total,
        suppressed,
        "loopback: /activity/discovery_skipped — a child skipped candidate discovery entries \
         and reached this instance anyway"
    );
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: agent,
        scope: &scope,
        attribution,
        session: session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root,
        detail: &row.detail,
    });
}

/// A discovery report's `injection_flag` row, composed by a **pure** function so
/// what an incident reader is told is assertable without an `AppHandle` — the
/// same seam [`beacon_row`] and [`override_row`] exist for.
///
/// The prose carries six facts, and none of them is optional:
///
/// 1. what happened (a child skipped N candidate entries that did not answer a
///    token-authenticated `GET /health`);
/// 2. that **containment worked, and this row is the proof** — the child reached
///    *this* instance anyway, which is how the report arrived at all;
/// 3. the benign cause (an unclean shutdown leaves `.cimp-discovery/<pid>.json`
///    behind; removal is graceful-exit only);
/// 4. the hostile cause (this is also exactly what a **planted** entry looks
///    like — #48 F-11/F-28, locked decision 30);
/// 5. what to do (list `.cimp-discovery/` next to the cImp executable), together
///    with what the app observed there itself;
/// 6. the **honesty clause**: an authenticated POST from a local process is not
///    evidence of a user action, the count is caller-asserted, and nothing here
///    moved a latch, contaminated a conversation or refused a call.
fn discovery_row(origin: outbound::Origin, rep: &DiscoveryReport) -> FlagRow {
    let n = rep.skipped;
    let clamped = if rep.clamped {
        format!(
            " (the caller's number exceeded the probe budget of {MAX_DISCOVERY_PROBES} and was \
             clamped)"
        )
    } else {
        String::new()
    };
    let stands_for = if rep.suppressed > 0 {
        format!(
            " This is report {} for this scope and stands for {} further report(s) folded into \
             it (rows are written at 1, 2, 4, 8 … so a loop cannot evict this lane's history).",
            rep.total, rep.suppressed
        )
    } else {
        String::new()
    };
    FlagRow {
        screen: outbound::Screen::DiscoverySkipped,
        origin,
        tool: DISCOVERY_TOOL.to_string(),
        detail: format!(
            "DISCOVERY ENTRY SKIPPED (origin: {}): a cImp MCP child resolved its loopback \
             endpoint and skipped {n} candidate discovery entr(ies) that did not answer a \
             token-authenticated `GET /health`{clamped} — and then reached THIS instance anyway, \
             which is how this report arrived. Containment worked; this row is the proof, not an \
             alarm about a failure. After an unclean cImp shutdown a leftover \
             `.cimp-discovery/<pid>.json` produces exactly this and is harmless (removal is \
             graceful-exit only). It is ALSO what a PLANTED entry looks like: a well-formed file \
             naming a deeper project root and a port nothing serves is how untrusted content \
             steers a child onto a dead endpoint (#48 F-11/F-28, locked decision 30). If you did \
             not expect it, list `.cimp-discovery/` next to the cImp executable — this app sees \
             {} entr(ies) there right now, {} of them not its own process.{stands_for} This row \
             records an authenticated POST from a local process: the launch token is readable by \
             anything running as this user, so it is NOT evidence of a user action, and the count \
             is CALLER-ASSERTED (clamped to the probe budget of {MAX_DISCOVERY_PROBES}; the \
             directory figures above are the app's own observation and are not). Nothing here \
             moved a latch, contaminated a conversation or refused a call.",
            origin.as_str(),
            rep.observed.entries,
            rep.observed.foreign,
        ),
    }
}

// ── NC-2 (issue #5): hook-driven permission detection ────────────────────────

/// A `POST /permission/event` request body — the Claude `--notify-hook` shim
/// forwarding a `Notification` or `PermissionDenied` hook payload.
#[derive(Deserialize, Default)]
struct PermissionEventBody {
    /// The hook payload's `cwd` (already resolved by the shim).
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// `~/.claude/projects/<slug>/<session_id>.jsonl` — the second mapping key.
    #[serde(default)]
    transcript_path: Option<String>,
    /// The payload's `hook_event_name` (`"Notification"` / `"PermissionDenied"`).
    #[serde(default)]
    event: String,
    /// The notification's type when the payload carries one (`permission_prompt`,
    /// `idle_prompt`, …).
    #[serde(default)]
    notification_type: Option<String>,
    /// The notification's prose, used to classify when no type field arrived.
    #[serde(default)]
    message: Option<String>,
    /// Present on `PermissionDenied`; logged, not branched on.
    #[serde(default)]
    tool_name: Option<String>,
}

/// Which edge of the existing `awaiting_permission` flag a hook payload maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionEdge {
    /// A permission prompt is now on screen ⇒ `PermissionPromptDetected`.
    Detected,
    /// The pending call was denied ⇒ `PermissionPromptResolved`.
    Resolved,
}

/// Substrings that identify a permission notification when the payload carries
/// no notification-type field. Claude Code's permission notification reads
/// "Claude needs your permission to use <tool>", so both fragments are checked
/// (either wording survives a small rephrasing). Deliberately narrower than a
/// bare `"permission"` test, which a "permission denied" filesystem-error
/// notification would trip.
const PERMISSION_MESSAGE_MARKERS: [&str; 2] = ["your permission", "permission to use"];

/// The notification type that means "a permission prompt is on screen" — the
/// value Claude Code's `Notification` matcher filters on.
const PERMISSION_NOTIFICATION_TYPE: &str = "permission_prompt";

/// Notification types we KNOW about and deliberately ignore (Claude Code hooks
/// guide, same list the `Notification` matcher accepts, minus
/// `permission_prompt`). Recognizing them by name is what lets an
/// *unrecognized* type fall through to the prose check without idle/auth
/// notifications riding along — see [`classify_permission_event`].
const IGNORED_NOTIFICATION_TYPES: [&str; 7] = [
    "idle_prompt",
    "auth_success",
    "elicitation_dialog",
    "elicitation_complete",
    "elicitation_response",
    "agent_needs_input",
    "agent_completed",
];

/// NC-2: map a hook payload to a permission edge, or `None` to ignore it.
///
///   * `PermissionDenied` (auto-classifier blocked the call) resolves the
///     prompt. Note the docs describe this as the auto-mode classifier's own
///     denial, NOT necessarily the user pressing "No" — treating it as a
///     resolution is still right (nothing is awaiting the user afterwards).
///     M11 fix (2026-08-05 review): the eager clear is paired with a
///     force-clear of that tab's regex latch in `handle_permission_event`, so a
///     prompt that is genuinely still on screen is re-raised on the detector's
///     next scan instead of staying invisible until the next keystroke.
///   * `Notification` is classified by its TYPE when the payload carries a
///     RECOGNIZED one (the field the matcher filters on), else by its prose.
///
/// **Type dispatch, in order (M12 fix, 2026-08-05 review):**
///   1. `permission_prompt` ⇒ `Detected`.
///   2. A type in [`IGNORED_NOTIFICATION_TYPES`] ⇒ ignored, prose NOT
///      consulted. Deliberate: see the idle note below.
///   3. Anything else — including an empty/absent type — falls through to the
///      prose check. This is the drift path the shim's payload-shape note calls
///      UNVERIFIED: a renamed type or a nested/renamed field must degrade to
///      "we read the message instead", never to silence. Returning early on
///      every unrecognized non-empty type inverted the contract precisely for
///      the permission case, where "ignored" IS silence.
///
/// **Idle notifications are deliberately dropped** — and, per rule 2, dropped
/// even when their prose would match. `idle_prompt` ("waiting for your input")
/// is semantically close to the `awaiting_question` pipe, but that pipe's
/// meaning today is "an AskUserQuestion-style menu is on screen" and the regex
/// detector owns it; wiring idle there would flip the badge/TTS on every turn
/// boundary. Revisit only with a separate signal.
fn classify_permission_event(
    event: &str,
    notification_type: &str,
    message: &str,
) -> Option<PermissionEdge> {
    match event {
        "PermissionDenied" => Some(PermissionEdge::Resolved),
        "Notification" => {
            let kind = notification_type.trim();
            if kind.eq_ignore_ascii_case(PERMISSION_NOTIFICATION_TYPE) {
                return Some(PermissionEdge::Detected);
            }
            if IGNORED_NOTIFICATION_TYPES
                .iter()
                .any(|t| kind.eq_ignore_ascii_case(t))
            {
                return None;
            }
            // Unrecognized (or absent) type ⇒ the prose is all we have.
            let msg = message.to_ascii_lowercase();
            PERMISSION_MESSAGE_MARKERS
                .iter()
                .any(|m| msg.contains(m))
                .then_some(PermissionEdge::Detected)
        }
        _ => None,
    }
}

/// One tab a permission event could belong to: its id, the Claude session id it
/// is currently running (from the graph's live-session registry — `None` for a
/// configured-but-not-running tab), and the directory it launches in.
#[derive(Debug, Clone)]
struct PermissionTabCandidate {
    tab: String,
    session_id: Option<String>,
    cwd: PathBuf,
}

/// NC-2: resolve a hook payload to exactly one tab, or `None` to DROP the event.
///
/// Fallback order — `session_id` → `transcript_path` → unique `cwd`:
///
///   1. **session id.** The live-session registry maps a Claude tab id to the
///      session its transcript tail last saw; a hook payload names that same id.
///   2. **transcript path.** The transcript filename stem IS the session id
///      (`<slug>/<session_id>.jsonl`), so this recovers the match when the
///      `session_id` field itself goes missing/renamed.
///   3. **cwd**, and only when it identifies exactly ONE Claude tab. Tabs
///      normally all inherit the app's launch dir, so this usually resolves only
///      for a single-Claude-tab setup (or a worktree tab with its own cwd).
///
/// Never guesses: an ambiguous or unmatched payload returns `None` and the event
/// is dropped, leaving detection to the TUI-regex fallback. Guessing would flip
/// the badge/TTS/avatar for the WRONG tab, which is worse than a missed hook.
///
/// H1 fix (2026-08-05 review): the `session_id` on a candidate is already
/// ambiguity-filtered upstream — with two RUNNING Claude tabs on one project the
/// registry withholds BOTH bindings (`graph::service::live_claude_tab_sessions`),
/// because the taps cannot tell those tabs' transcripts apart. Passes 1 and 2
/// then find nothing and pass 3 sees the shared cwd on ≥2 tabs, so the event is
/// dropped rather than attributed to whichever tab wrote last.
fn resolve_permission_tab(
    candidates: &[PermissionTabCandidate],
    session_id: &str,
    transcript_path: &str,
    cwd: &str,
) -> Option<String> {
    let by_session = |sid: &str| -> Option<String> {
        if sid.is_empty() {
            return None;
        }
        let mut hits = candidates
            .iter()
            .filter(|c| c.session_id.as_deref() == Some(sid));
        let first = hits.next()?;
        // A session id belongs to one tab; if two tabs somehow claim it, refuse
        // rather than pick.
        hits.next().is_none().then(|| first.tab.clone())
    };

    if let Some(tab) = by_session(session_id) {
        return Some(tab);
    }
    if let Some(tab) = transcript_session_id(transcript_path).and_then(|s| by_session(&s)) {
        return Some(tab);
    }
    let target = norm_dir(cwd)?;
    let mut hits = candidates
        .iter()
        .filter(|c| norm_dir(&c.cwd.to_string_lossy()).as_deref() == Some(target.as_str()));
    let first = hits.next()?;
    hits.next().is_none().then(|| first.tab.clone())
}

/// The session id encoded in a Claude transcript path
/// (`…/projects/<slug>/<session_id>.jsonl`), or `None` for an empty/odd path.
fn transcript_session_id(transcript_path: &str) -> Option<String> {
    let stem = Path::new(transcript_path.trim()).file_stem()?;
    let stem = stem.to_string_lossy().into_owned();
    (!stem.is_empty()).then_some(stem)
}

/// A directory string normalized for comparison: separators unified, trailing
/// separators dropped, and — on Windows, whose paths are case-insensitive —
/// case-folded. `None` for an empty/whitespace path.
///
/// H1-R5 fix: delegates to [`crate::fsutil::norm_dir_key`], which the H1
/// ambiguity predicate's transcript-root key also goes through — the two seams
/// compare "same project dir?" and must not drift apart.
fn norm_dir(dir: &str) -> Option<String> {
    crate::fsutil::norm_dir_key(dir)
}

/// `POST /permission/event` (NC-2): the hook-driven half of permission
/// detection. Maps the payload to a tab and emits the SAME `StateSignal`s the
/// TUI-regex detector emits, so the whole downstream pipeline
/// (`awaiting_permission` → TTS enqueue, per-tab badge, avatar) is untouched.
/// Both producers are idempotent at the state manager, so a hook and a regex
/// match for the same prompt collapse to one edge.
///
/// Always answers 200 `{ok:true}` (with a `mapped` field for diagnosis): the
/// shim ignores the response and must never be given a reason to retry.
async fn handle_permission_event(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: PermissionEventBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let session_id = body.session_id.unwrap_or_default();
    let transcript_path = body.transcript_path.unwrap_or_default();
    let cwd = body.cwd.unwrap_or_default();
    let Some(edge) = classify_permission_event(
        &body.event,
        body.notification_type.as_deref().unwrap_or(""),
        body.message.as_deref().unwrap_or(""),
    ) else {
        debug!(
            event = %body.event,
            kind = body.notification_type.as_deref().unwrap_or(""),
            "permission hook: ignored (not a permission edge)"
        );
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "mapped": false, "reason": "ignored" }),
        )
        .await;
    };

    // Snapshot everything we need from managed state, then drop the guards —
    // nothing borrowed from `AppHandle` is held across the response write.
    let resolved = app.try_state::<crate::ipc::AppState>().map(|state| {
        let sessions: Vec<(String, String)> = app
            .try_state::<Arc<crate::graph::GraphService>>()
            .map(|g| g.live_claude_sessions())
            .unwrap_or_default();
        let candidates: Vec<PermissionTabCandidate> =
            crate::tabs::claude_tab_dirs(&state.settings.current(), &state.launch.cwd)
                .into_iter()
                .map(|(tab, dir)| PermissionTabCandidate {
                    session_id: sessions
                        .iter()
                        .find(|(k, _)| *k == tab)
                        .map(|(_, s)| s.clone()),
                    tab,
                    cwd: dir,
                })
                .collect();
        (
            resolve_permission_tab(&candidates, &session_id, &transcript_path, &cwd),
            state.state_signals.clone(),
            state.tabs.clone(),
        )
    });
    let Some((Some(tab), signals, registry)) = resolved else {
        debug!(
            event = %body.event,
            session = %session_id,
            cwd = %cwd,
            "permission hook: no unambiguous tab — dropped (regex fallback still covers it)"
        );
        return write_json(
            stream,
            200,
            &serde_json::json!({ "ok": true, "mapped": false, "reason": "no tab" }),
        )
        .await;
    };

    let tab_id = crate::state::TabId::from_str(&tab);
    let signal = match edge {
        PermissionEdge::Detected => {
            crate::state::StateSignal::PermissionPromptDetected {
                tab: tab_id.clone(),
            }
        }
        PermissionEdge::Resolved => {
            crate::state::StateSignal::PermissionPromptResolved {
                tab: tab_id.clone(),
            }
        }
    };
    // Edge-triggered and best-effort, exactly like the PTY processor's
    // `try_send`: a full channel means the state manager is saturated, and the
    // regex detector's next scan re-raises the edge anyway.
    let _ = signals.try_send(signal);
    // M11 (2026-08-05 review): a hook-driven Resolved clears the flag eagerly —
    // a `PermissionDenied` from the auto-classifier can land while a genuine
    // approval prompt is still on screen. The regex fallback cannot recover on
    // its own: `PermissionDetector::check` is edge-triggered on a latched
    // per-kind pattern name, so while that same pattern keeps matching it emits
    // NOTHING. Drop the latch (and re-scan) in the tab's PTY processor so a
    // prompt that is genuinely still up is re-raised immediately. Sent AFTER
    // the Resolved signal so the two land on the state manager in that order.
    if matches!(edge, PermissionEdge::Resolved) {
        registry.lock().await.clear_permission_latch(&tab_id).await;
    }
    info!(
        event = %body.event,
        tool = body.tool_name.as_deref().unwrap_or(""),
        ?edge,
        %tab,
        "permission hook: state signal sent"
    );
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "mapped": true, "tab": tab }),
    )
    .await
}

/// A `POST /context/post_edit` request body (the Claude `PostToolUse` shim, or
/// the OpenCode plugin's `tool.execute.after` hook).
#[derive(Deserialize)]
struct ContextPostEditBody {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    file_path: String,
    /// Recorded for symmetry with the shim's payload; not currently branched
    /// on (the matcher/plugin already scope this to edit-class tools).
    #[serde(default)]
    #[allow(dead_code)]
    tool_name: Option<String>,
    /// #48 (M-7): which shim is calling — `"claude"` (the `--postedit-hook`
    /// shim) or `"opencode"` (the generated plugin). See [`hook_agent`].
    #[serde(default)]
    agent: Option<String>,
    /// #48 (M-7): the cImp TAB this hook serves — `--tab <id>` from argv on the
    /// Claude side, `CIMP_TAB_ID` on the OpenCode side.
    #[serde(default)]
    tab: Option<String>,
}

/// **V33 C4** — every directory this instance will run the project's configured
/// CHECK COMMANDS in on a hook's behalf: the **served root** (this app's launch
/// directory) plus each **configured AI tab's** working directory, and nothing
/// else. Derived entirely from the app and the settings snapshot; the request
/// body contributes nothing to this list.
///
/// The tab dirs are here because they are not always under the launch root: V13
/// Phase D's "New tab in worktree…" sets `AiToolTabConfig::cwd` to a freshly
/// created git worktree, and a hook firing in that tab legitimately names it.
/// Resolution is [`crate::tabs::ai_tab_dir`], the same call
/// [`build_ai_tool_spec`](crate::tabs::config) makes when it actually spawns the
/// tab, so this list is the set of directories cImp itself launches agents in.
///
/// Every consumer's tabs, not the caller's: these are the operator's own
/// directories either way, and scoping the list by the caller's asserted
/// `agent` would let the assertion move a *capability* boundary — the thing
/// C5 exists to stop it doing to the identity one.
///
/// An empty vec is possible only when managed state is absent AND
/// `current_dir()` fails (a deleted cwd). It denies everything, which is the
/// correct answer: a root that cannot be resolved must read as absent, never as
/// "allow whatever was asked for".
fn hook_exec_roots(app: &AppHandle, settings: &crate::settings::Settings) -> Vec<PathBuf> {
    let launch = app
        .try_state::<crate::ipc::AppState>()
        .map(|s| s.launch.cwd.clone())
        .or_else(|| std::env::current_dir().ok());
    let Some(launch) = launch else {
        return Vec::new();
    };
    let mut roots = vec![launch.clone()];
    for tab in ai_tab_ids(settings) {
        if let Some(dir) = crate::tabs::ai_tab_dir(settings, tab, &launch) {
            if !roots.contains(&dir) {
                roots.push(dir);
            }
        }
    }
    roots
}

/// **V33 C4** — the working directory `POST /context/post_edit` may execute in,
/// or `None` to refuse.
///
/// This is [`audit_admit`]'s step 3 in a second place, deliberately built from
/// the same two helpers ([`canon`] + [`is_ancestor_or_equal`]) rather than from
/// new path logic, so the two routes' notions of "inside a root I serve" cannot
/// drift. What differs is only the answer to a miss: `/audit/run` returns a
/// readable tool error, and this route — a hook that must never perturb an edit
/// — returns its own fail-safe (empty text) with an operator-visible `warn!`.
///
/// Three cases:
///
/// 1. **No `cwd` on the wire** ⇒ the served root. The pre-V33 default was
///    `PathBuf::from(".")`, i.e. the app process's cwd; the served root is that
///    same directory by a route that cannot be moved by a `chdir` and that is
///    stated rather than implied.
/// 2. **A `cwd` at or under one of the roots** ⇒ admitted, and passed through
///    **as written**. The path string keys the single-flight `RootRunner`
///    bucket and the auto-check baseline downstream, so canonicalizing it here
///    would silently re-bucket every existing caller.
/// 3. **Anything else** ⇒ `None`. Including a path containing `..`, which is
///    refused inside [`is_ancestor_or_equal`] rather than here: a component walk
///    cannot resolve a `..`, and [`canon`] only resolves one for a path that
///    EXISTS, so an unresolved `..` reaching a zip-compare reads as a
///    descendant. That refusal is shared with [`audit_admit`] step 3
///    deliberately — see the helper's own note for the measurement behind it,
///    including why the Windows spelling in this comment's first draft
///    (`P:\served\..\..\evil`) was rejected for the wrong reason and
///    `\\?\P:\served\..\..\evil` was not rejected at all. Costs nothing here:
///    every real caller sends the absolute cwd its harness reported.
fn admitted_hook_root(roots: &[PathBuf], requested: Option<&str>) -> Option<PathBuf> {
    let Some(req) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return roots.first().cloned();
    };
    let hint = canon(Path::new(req));
    roots
        .iter()
        .any(|r| is_ancestor_or_equal(&canon(r), &hint))
        .then(|| PathBuf::from(req))
}

/// `POST /context/post_edit` (V12 Phase F): debounce this session's edits, run
/// the project's configured checks single-flight per root, diff against the
/// session's own baseline, and return only NEW/worsened diagnostics (plus an
/// optional auto-impact note) as `{ ok, text }`. Fails open to empty text on
/// any missing state — the hook must never block or perturb an edit.
///
/// #48 (M-7): gated through [`hook_admit`] on [`HOOK_TOOL_POST_EDIT`]. This is
/// the route the finding is really about — it **executes the project's
/// configured check commands**, which is the definition of LOCAL-CAPABILITY
/// under decision 1, and it did so with no `latches()` call at all. A refusal
/// answers with the route's own fail-safe (empty text), so a contaminated
/// conversation loses its auto-check diagnostics and nothing else; the edit
/// itself is never perturbed.
///
/// **V33 C4 closes the directory half.** The `cwd` those commands run in used
/// to come straight out of the request body (defaulting to `"."`) with no
/// ancestor check and no allowlist, so anything holding the loopback token could
/// have the operator's own vetted check commands executed in a directory it
/// named — a cloned repo's `Makefile`, say, reached through a `cargo`/`npm`
/// script the operator configured for their own project. It is now resolved through
/// [`admitted_hook_root`] against [`hook_exec_roots`], which derives from the
/// served root and the configured tabs and **never from the request**. A refusal
/// takes the route's own fail-safe (empty text) and logs; it cannot perturb the
/// edit.
///
/// **The identity half is deliberately untouched** (locked V33 decision 2). A
/// body with no usable `tab` still resolves to no scope and is ADMITTED, exactly
/// as on `/graph_run` and `/mcp/call` — see the residual note above `hook_admit`.
/// The two halves are independent: C4's allowlist is app-derived, so omitting
/// `tab` does not walk around it, which is why the directory half could be
/// closed without settling the identity one.
///
/// **Why the sibling hook routes get no such check** (so the asymmetry is not
/// later read as an oversight): `/context/should_read` and
/// `/context/compaction` take the same caller-supplied `cwd` and share the same
/// identity fail-open, but neither EXECUTES anything with it — it selects which
/// project's index to read, and what a read can hand back is what their
/// [`toolclass::TABLE`] rows and their [`hook_admit`] gate already decide. There
/// is no command to run in a directory a caller names, so there is nothing for
/// a root allowlist to contain. If either ever grows a spawn, it inherits this
/// route's treatment.
async fn handle_post_edit(stream: &mut TcpStream, app: &AppHandle, req: &Request) -> AppResult<()> {
    let empty = serde_json::json!({ "ok": true, "text": "" });
    let body: ContextPostEditBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let settings = live_settings(app);
    if hook_admit(
        latches(),
        HOOK_TOOL_POST_EDIT,
        hook_agent(body.agent.as_deref()),
        body.tab.as_deref(),
        |agent, tab| latch_scope(app, &settings, agent, tab),
        |scope| GatePolicy::resolve(&settings, scope),
    )
    .is_err()
    {
        return write_json(stream, 200, &empty).await;
    }
    // V33 C4: decide WHERE before deciding whether there is anything to run —
    // the roots are app-derived, so this cannot be moved by the body.
    let Some(cwd) = admitted_hook_root(&hook_exec_roots(app, &settings), body.cwd.as_deref()) else {
        // Bounded: the rejected string is caller-chosen and unbounded on the
        // wire, and this is the one place it reaches an operator-facing line.
        warn!(
            target: "offload",
            requested = %bounded_id(body.cwd.as_deref().unwrap_or_default()),
            "loopback: /context/post_edit named a working directory outside this instance's \
             served root and its configured tabs' directories — the project's configured check \
             commands were NOT run there (the edit itself is unaffected)"
        );
        return write_json(stream, 200, &empty).await;
    };
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &empty).await;
    };
    let graph = graph.inner().clone();
    let text = graph
        .post_edit(&cwd, body.session_id.as_deref(), &body.file_path)
        .await
        .unwrap_or_default();
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "text": text }),
    )
    .await
}

/// A `POST /memory/event` request body (the OpenCode plugin's tool hook — the
/// only memory ingress for OpenCode, whose OOB SSE stream carries no tool
/// events, AND — V14 Phase C — the only *usage* ingress for OpenCode, for the
/// same reason). Claude records both in-process via the transcript tap instead.
#[derive(Deserialize)]
struct MemoryEventBody {
    #[serde(default)]
    cwd: Option<String>,
    session_id: String,
    #[serde(default)]
    agent: Option<String>,
    // Tool-event shape (V10): present on `tool.execute.after` POSTs. Optional
    // now that the same route also carries usage bodies (V24 Phase F), which
    // have no `tool`.
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args: Value,
    // V24 Phase F usage shape: `kind == "usage"`, emitted by the plugin's
    // `event` hook on a completed assistant turn.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    msg_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    in_tok: u32,
    #[serde(default)]
    out_tok: u32,
    #[serde(default)]
    cache_read: u32,
    #[serde(default)]
    cache_make: u32,
}

/// V24 Phase F: from an OpenCode usage POST body, the target session id and the
/// [`crate::graph::UsageEvent::Turn`] to record — or `None` when the body has no
/// usable data (missing/empty `msg_id`, or all four token totals are zero).
///
/// When `parent_session_id` is present the spend rolls up to the PARENT session
/// with `origin: Agent` (sub-agent spend is the parent's spend — mirrors the
/// Claude sub-agent contract); otherwise it's the reporting session with
/// `origin: Session`. A model id that is absent/empty maps to `None` (unknown
/// model), matching the Claude tap. Pure, so the mapping is unit-tested without
/// a live handler.
fn usage_event_from_body(body: &MemoryEventBody) -> Option<(String, crate::graph::UsageEvent)> {
    let msg_id = body.msg_id.clone().filter(|m| !m.is_empty())?;
    // The plugin only forwards COMPLETED turns, so an all-zero body is a
    // degenerate/creation emit — skip it rather than plant an empty turn row.
    // `est_only` is unaffected either way (it's derived from the summed token
    // totals, which a zero row doesn't move), so skipping only keeps the turn
    // series free of noise; it never resurrects real data.
    if body.in_tok == 0 && body.out_tok == 0 && body.cache_read == 0 && body.cache_make == 0 {
        return None;
    }
    let (target, origin) = match body.parent_session_id.as_deref() {
        Some(p) if !p.is_empty() => (p.to_string(), crate::graph::UsageOrigin::Agent),
        _ => (body.session_id.clone(), crate::graph::UsageOrigin::Session),
    };
    Some((
        target,
        crate::graph::UsageEvent::Turn {
            msg_id,
            model: body.model.clone().filter(|m| !m.is_empty()),
            in_tok: body.in_tok,
            out_tok: body.out_tok,
            cache_read: body.cache_read,
            cache_make: body.cache_make,
            origin,
        },
    ))
}

/// V24 Phase F: for a tool-event body (`tool.execute.after`), the parent
/// session id when the reporting session is a task-tool CHILD (sub-agent), else
/// `None`. A child's tool events mirror the Claude sidechain contract (see
/// `oob/claude.rs` `record_tool_events`, which early-returns on `isSidechain`):
/// they are dropped rather than recorded against the child, and only the parent
/// is marked live. Pure, so the routing is unit-tested without a live handler.
fn tool_event_parent(body: &MemoryEventBody) -> Option<String> {
    body.parent_session_id
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// V32 Phase F, C-2 token variant (2026-08-07 review): mark `session` live from
/// a `/memory/event` body, refusing any id that collides with a configured AI
/// tab id.
///
/// # Why the guard exists
///
/// `live_sessions` is ONE map with TWO key spaces: the Claude tap keys it by
/// **tab id** (`oob/claude.rs`), and OpenCode's loopback path keys it by the
/// **reporting session id**, because OpenCode has no tab binding here (V24
/// Phase B). Nothing kept them apart. `handle_memory_event` derives all three
/// of its keys from request-body strings, with `agent` defaulting to
/// `"opencode"` and no validation of any kind — #45's check is on the *read*
/// side (`latch_scope`), not here — so an authenticated POST could write
/// `live_sessions["claude-1"] = <attacker string>` and repoint a real tab's
/// session identity.
///
/// Two things then follow, and the second is the sharp one:
/// - V28 memory scoping is corrupted: `/graph_run`'s `context_*` calls for that
///   tab resolve to a session the tab never had.
/// - [`TabLatch::observe`] reads that session through the same lookup, sees a
///   *changed* id, and treats it as a new conversation — clearing the latch,
///   the budget **and `contaminated`**, which locked decision 15 says only a
///   genuinely new conversation may do. The real tap re-stamps the true id
///   within its 200 ms poll, producing a SECOND rotation, so the race helps the
///   attacker: POST in a loop and the tab flaps clean.
///
/// # Why rejection, and why exact-match
///
/// Namespacing the OpenCode key space would work equally well and is the other
/// option the review named, but it would rewrite the keys V24's usage/permission
/// consumers already read (`live_claude_sessions`, `compute_active_session_ids`)
/// for a hazard that only exists at the collision. Rejecting the collision
/// leaves every legitimate key untouched: a real OpenCode session id is a UUID,
/// and a cImp tab id is config-derived (`claude`, `opencode-2`), so the two
/// never legitimately meet.
///
/// Exact-match against the configured list, with **no** empty-list escape (see
/// [`names_a_configured_ai_tab`]): "settings are not loaded yet" must not be a
/// window in which every string is refused, and a string that collides with
/// nothing is not an attack.
///
/// This closes the token-gated half of C-2 only. The filesystem half — a
/// zero-byte `.jsonl` appearing in the transcript dir — is closed in
/// `oob/claude.rs` by requiring observed growth before a rotated file is marked
/// live. **Neither alone is sufficient**: they are two independent writers into
/// the same registry.
///
/// `mark` is the registry write, taken as a parameter rather than reached
/// through a `GraphService` this crate has no `AppHandle` to build: the point of
/// #48's `only_configured_ai_tab_ids_can_ever_key_a_latch` rewrite is that a
/// bound asserted *beside* its enforcement point survives deleting the call, so
/// the test drives this function and observes whether the write happened.
fn mark_live_session_from_event(
    mark: impl FnOnce(&str),
    settings: &crate::settings::Settings,
    agent: &str,
    session: &str,
) {
    if names_a_configured_ai_tab(settings, session) {
        warn!(
            target: "offload",
            agent,
            key = %session,
            "loopback: /memory/event refused — the session id collides with a configured tab id"
        );
        return;
    }
    mark(session);
}

/// `POST /memory/event`: classify an agent tool call and record it as a memory
/// event, AND (V14 Phase C) record its estimated usage. Best-effort — an
/// unclassifiable tool or a missing graph service is a silent no-op (200 with
/// memory recording skipped; usage recording no-ops internally the same way),
/// never an error the plugin has to handle.
async fn handle_memory_event(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: MemoryEventBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return write_json(
                stream,
                400,
                &serde_json::json!({ "ok": false, "error": format!("bad request body: {e}") }),
            )
            .await;
        }
    };
    let ok = serde_json::json!({ "ok": true });
    let Some(graph) = app.try_state::<Arc<crate::graph::GraphService>>() else {
        return write_json(stream, 200, &ok).await;
    };
    let graph = graph.inner().clone();
    let cwd = body
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let agent = body.agent.as_deref().unwrap_or("opencode");
    // C-2 (2026-08-07 review): ONE settings read for the whole request, feeding
    // every `mark_live_session` below through `mark_live_session_from_event` —
    // see its docs for why a body-supplied key must not be able to name a tab.
    let settings = live_settings(app);

    // V24 Phase F: the usage arm — a completed assistant turn's real token
    // totals (OpenCode's only exact-token ingress; see the spike note atop
    // `oob/opencode.rs`). Distinct body shape (`kind == "usage"`, no `tool`),
    // so it short-circuits the tool-event path below.
    if body.kind.as_deref() == Some("usage") {
        if let Some((target, event)) = usage_event_from_body(&body) {
            // Roll-up target = the parent when a child (sub-agent) session
            // reported, else the reporting session itself. `record_usage`
            // upserts by `msg_id`, so the plugin's duplicate final emit is
            // harmless.
            graph.record_usage(&cwd, &target, agent, event);
            // Mark the SAME id live: the target is the session row that exists
            // / gets the spend attributed (the parent when a child reports),
            // so that's the row the Sessions list should flag active.
            mark_live_session_from_event(
                |k| graph.mark_live_session(k, agent, k),
                &settings,
                agent,
                &target,
            );
        }
        return write_json(stream, 200, &ok).await;
    }

    // Tool-event path (V10): requires a `tool` name. A body without one and not
    // a usage event has nothing to record.
    let Some(tool_name) = body.tool.clone() else {
        return write_json(stream, 200, &ok).await;
    };

    // V24 Phase F: a task-tool CHILD (sub-agent) session's tool events are the
    // sub-agent's own working set, not the parent's — mirror the Claude sidechain
    // contract (oob/claude.rs `record_tool_events` early-returns on isSidechain)
    // and drop them entirely: no mem event, no tool-result chars against the
    // child, and the child is never marked live. The child's real token spend
    // still reaches the parent via the usage arm above; mark the PARENT live so
    // the sub-agent's activity keeps the parent's row active.
    if let Some(parent) = tool_event_parent(&body) {
        mark_live_session_from_event(
            |k| graph.mark_live_session(k, agent, k),
            &settings,
            agent,
            &parent,
        );
        return write_json(stream, 200, &ok).await;
    }

    if let Some((kind, arg)) = crate::graph::classify_tool(&tool_name) {
        let get = |k: &str| body.args.get(k).and_then(Value::as_str);
        let (path, detail) = match arg {
            crate::graph::MemArg::Path => (
                get("file_path")
                    .or_else(|| get("filePath"))
                    .or_else(|| get("notebook_path"))
                    .or_else(|| get("path"))
                    .unwrap_or("")
                    .to_string(),
                None,
            ),
            crate::graph::MemArg::Pattern => (
                get("pattern")
                    .or_else(|| get("path"))
                    .or_else(|| get("query"))
                    .unwrap_or("")
                    .to_string(),
                None,
            ),
            crate::graph::MemArg::Command => (
                String::new(),
                get("command").map(|c| c.chars().take(200).collect::<String>()),
            ),
        };
        // Skip an event with no usable target: an empty path (Path/Pattern) or a
        // Command whose `command` arg was absent (detail is None) — recording it
        // would just evict useful events from the ring.
        let recordable = match arg {
            crate::graph::MemArg::Command => detail.is_some(),
            _ => !path.is_empty(),
        };
        if recordable {
            graph.record_mem_event(
                &cwd,
                &body.session_id,
                agent,
                kind,
                &path,
                None,
                None,
                detail.as_deref(),
            );
        }
    }

    // V14 Phase C: OpenCode's usage tap (see the C3 spike note atop
    // `oob/opencode.rs` — its SSE stream carries no usage fields, so this
    // hook, which already fires after every tool call, is the only place
    // that can record OpenCode usage). Unlike the memory recording above,
    // this runs for EVERY tool call, not just ones `classify_tool` maps to a
    // filesystem/query target — usage wants the full picture. `chars` is
    // estimated from the tool's serialized INPUT args (its actual output
    // isn't visible to this hook). This path records only tool-result chars,
    // never Turn tokens, so a session that never got a real usage event stays
    // est-only in the X-ray (V24 Phase E derives `est_only` from zero token
    // totals — see `usage_row_for_session`).
    let chars = serde_json::to_string(&body.args)
        .map(|s| s.chars().count())
        .unwrap_or(0) as u32;
    graph.record_usage(
        &cwd,
        &body.session_id,
        agent,
        crate::graph::UsageEvent::ToolResult {
            tool: Some(tool_name.clone()),
            chars,
        },
    );

    // V24 Phase B: OpenCode has no tab binding on this path, so the live-session
    // registry is keyed by the reporting session id itself; the entry expires by
    // TTL (there is no cancel signal to clear it — see the C3 spike note atop
    // `oob/opencode.rs`). C-2: which is exactly why the key must not be allowed
    // to name a TAB — the other half of the same map.
    mark_live_session_from_event(
        |k| graph.mark_live_session(k, agent, k),
        &settings,
        agent,
        &body.session_id,
    );

    write_json(stream, 200, &ok).await
}

/// `POST /mcp/list`: the proxied MCP tool descriptors for the requesting
/// consumer (servers with that consumer's access flag), for the per-session
/// child to merge into its `tools/list`. The consumer is taken from the
/// `?consumer=` query (Claude when absent). Returns
/// `{ "tools": [ {name, description, inputSchema}, … ] }`.
async fn handle_mcp_list(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let tools = service.mcp_tool_descriptors(consumer_of(req)).await;
    write_json(stream, 200, &serde_json::json!({ "tools": tools })).await
}

/// `POST /mcp/call`: run one proxied MCP tool for the requesting consumer.
/// Body `{name, arguments}`; consumer from `?consumer=` (Claude when absent).
/// The service guards the call against any tool not offered by a server
/// exposed to that consumer. 200 even on a tool-level error (the child renders
/// `error` as a tool result).
///
/// V32 Phase B adds the two halves of the consumer-side containment: the tab's
/// session taint latch in front of the call, and the spotlighting envelope
/// around its result. This is the tab's untrusted-content intake — the one
/// route through which a fetched page's bytes reach a Claude/OpenCode session.
async fn handle_mcp_call(
    stream: &mut TcpStream,
    service: &Arc<OffloadService>,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: McpCallBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    // Normalized through the same `source_for_consumer` vocabulary
    // `/graph_run` uses, so one tab keys one latch from both routes.
    let agent =
        crate::graph::source_for_consumer(query_param(&req.path, "consumer").unwrap_or("claude"));
    // V32 Phase G: ONE settings read here, so the tab-id check, the latch, the
    // budget, detection and the envelope all resolve under the same snapshot —
    // a mid-call settings save must not leave a result screened by one posture
    // and wrapped by another. #45 adds the tab-id check inside `latch_scope` to
    // that list, which is why the read precedes it.
    //
    // Corrected 2026-08-08 (#48, review finding G-4): this comment also named
    // "the SSRF screen", and that one is NOT resolved here. `ServiceHandle::mcp_call`
    // builds `outbound::Policy` from its own independent `self.settings.current()`,
    // so the SSRF guard can be a snapshot behind or ahead of everything above.
    // Benign in practice (sub-millisecond, both postures the user's own) and
    // recorded as an accepted residual in the V32 spec rather than fixed here;
    // the fix is to thread `settings` into `mcp_call`. Do not restore the old
    // wording without making it true.
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    // #48 F-20 — see `handle_graph_run`: resolved before the collapse. This is
    // the row that answers "which tab fetched that page", and it is the one the
    // finding says could not.
    let tab_attr = scoping.attribution();
    let scope = scoping.into_scope();
    let inj_scope = crate::settings::injection::Scope::for_tab(agent, body.tab.as_deref());
    let gate_policy = GatePolicy::resolve(&settings, scope.as_ref());
    // V32 Phase C: the flagged row's provenance, read from the arguments before
    // they are moved into the call — the result alone cannot say which page it
    // came from, and that is the first thing a user reads off the row.
    //
    // #48 (F-3): read BEFORE the gate rather than after the budget, because the
    // gate is now also a row writer — this is the route whose admitted call
    // contaminates the conversation, and "from which page" is one of the three
    // facts the contamination row exists to carry. Moving the read up changes
    // nothing else: `origin_of` only inspects the arguments.
    let (flag_url, flag_host) = detection::origin_of(&body.arguments);
    // The gate's V32 Phase C2 `WriteTaint` is discarded here, and can only ever
    // be `Clean`: this route serves proxied `<server>__<tool>` ids, every one of
    // which classifies EXTERNAL by the unknown-⇒-EXTERNAL invariant, so no
    // PERSISTENT-WRITE can arrive on it. Memory writes reach cImp through
    // `/graph_run` alone.
    if let Err(refusal) = latches().gate(
        scope.as_ref(),
        LatchRoute::Proxied,
        &body.name,
        gate_policy,
        CallProvenance::intake(flag_url.as_deref(), flag_host.as_deref()),
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }
    // V32 Phase C: the session's EXTERNAL budget, checked after the latch (a
    // latched-out call was never going to run, and must not consume the one
    // budget report) and before the call leaves the process.
    if let Err(refusal) = latches().budget_gate(
        scope.as_ref(),
        crate::settings::injection::budget_limits(&settings, inj_scope),
        &body.name,
    ) {
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(refusal.to_string()),
        };
        return write_json(stream, 200, &r).await;
    }

    let cwd = body.cwd.map(PathBuf::from);
    // The scope label the SSRF screen's `injection_flag` row carries. Without
    // tab identity we are already fail-open on the latch and the budget; the
    // SSRF screen still runs (it needs no identity), so name the scope honestly
    // rather than inventing one.
    let scope_label = scope
        .as_ref()
        .map(LatchScope::label)
        .unwrap_or_else(|| format!("{agent}:{}", outbound::NO_TAB_IDENTITY));
    let detection_cfg = detection::Config::from_settings(&settings, inj_scope);
    let spotlight_on = crate::settings::injection::effective(
        crate::settings::injection::Feature::Spotlighting,
        inj_scope,
        &settings,
    );
    let root_key = cwd
        .as_deref()
        .map(crate::activity::root_key)
        .unwrap_or_default();
    // #48: the tab session's audit-row claim ledger, threaded to the SSRF
    // chokepoint and to the detection boundary so neither can flood a capped
    // feed on a loop. See `TabAudit`.
    let audit = TabAudit(scope.as_ref(), agent);
    let called = service
        .mcp_call(
            consumer_of(req),
            &body.name,
            body.arguments,
            cwd.as_deref(),
            &scope_label,
            body.tab.as_deref(),
            tab_attr,
            &audit,
        )
        .await;
    // V32 Phase C, corrected in #48 (D-3): charge the session's EXTERNAL budget
    // for the call that was just ATTEMPTED — before the match, so it cannot
    // again end up on one arm only. See `LatchRegistry::charge_call`.
    latches().charge_call(scope.as_ref(), &called);
    let r = match called {
        // Locked decisions 5 + 6: detection, the envelope and the warning
        // header all compose here, at the proxy's tool-result boundary, so
        // EVERY consumer gets them identically — and they apply whether or not
        // the call carried tab identity, since none of the three needs it. The
        // same `wrap_external_result` the worker's boundary calls, so the
        // external-only rule and the composition order have one definition.
        //
        // #48 M-17 corrects the sentence that used to end this comment — "Errors
        // are cImp-composed strings, not fetched content, and are never screened
        // or wrapped." The diagnostic half is cImp's; the server's own
        // `error.message` never was, and it reached the model here with no bound,
        // no envelope and no screen. `HostError` keeps the two halves apart and
        // `wrap_remote_error` treats the remote half as what it is.
        Ok(text) => {
            let wrapped = detection::wrap_external_result(
                &body.name,
                text,
                detection::ResultCtx {
                    consumer: agent,
                    scope: &scope_label,
                    root: root_key,
                    url: flag_url,
                    host: flag_host,
                    cfg: detection_cfg,
                    spotlight: spotlight_on,
                    audit: &audit,
                    // #48/M-5: the proxy truncates NOTHING after this point — the
                    // consumer (a Claude/OpenCode tab) receives the whole result.
                    // This is the boundary where the unscreened notice is
                    // load-bearing, and the reason it is derived rather than
                    // deleted.
                    delivered_bytes: usize::MAX,
                },
            )
            .await;
            RunResult {
                ok: true,
                text: Some(wrapped),
                error: None,
            }
        }
        Err(e) => RunResult {
            ok: false,
            text: None,
            error: Some(
                detection::wrap_remote_error(
                    &body.name,
                    e.diagnostic(),
                    e.remote(),
                    detection::ResultCtx {
                        consumer: agent,
                        scope: &scope_label,
                        root: root_key,
                        url: flag_url,
                        host: flag_host,
                        cfg: detection_cfg,
                        spotlight: spotlight_on,
                        audit: &audit,
                        // As above: nothing truncates a proxied error either.
                        delivered_bytes: usize::MAX,
                    },
                )
                .await,
            ),
        },
    };
    write_json(stream, 200, &r).await
}

/// A `POST /latch/beacon` body — V32 Phase F (locked decision 14).
///
/// Posted by the `cimp --taint-beacon` Claude `PreToolUse` shim and by the
/// OpenCode plugin's `tool.execute.before` handler when the model reaches for a
/// HARNESS-NATIVE web tool. Every field except `tab` is descriptive; `tab` is
/// the only one the latch actually needs.
#[derive(Deserialize)]
struct LatchBeaconBody {
    /// The cImp tab id the reporting harness was spawned for. Absent ⇒
    /// fail-open, exactly like [`GraphRunBody::tab`].
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer` so one
    /// tab keys the same latch from every route.
    #[serde(default)]
    consumer: Option<String>,
    /// The native tool that is about to run (`WebFetch`, `webfetch`, …). Log
    /// and diagnostics only.
    #[serde(default)]
    tool: Option<String>,
}

/// A `POST /latch/state` body — V32 Phase H (locked decision 17).
///
/// Same two identity fields as [`LatchBeaconBody`] and nothing else: the query
/// is "what is in force for this tab", and the answer must not depend on
/// anything the *caller* claims about the tool it is about to run.
#[derive(Deserialize)]
struct LatchStateBody {
    /// The cImp tab id. Absent ⇒ no scope ⇒ the fail-open answer (`gate:false`).
    #[serde(default)]
    tab: Option<String>,
    /// `claude` / `opencode`, normalized through `source_for_consumer`.
    #[serde(default)]
    consumer: Option<String>,
}

/// `POST /latch/beacon`: engage a tab's EXTERNAL latch because the HARNESS's
/// own web tool is about to run (locked decision 14, sensor mode).
///
/// Behind the same bearer token as every other route — an unauthenticated
/// caller must not be able to latch a tab out of its local tools, which would
/// be a denial-of-service on the user's session dressed as containment.
///
/// **This is the route #45 left reachable**, and the reasoning is the asymmetry
/// between what it can do and what the removed override route could. A beacon
/// only ever TIGHTENS: Open → External, plus the contamination bit. It cannot
/// flip to Local, cannot unlatch, and cannot clear contamination. Its abuse case
/// is therefore a denial of the user's own local tools, recoverable by a tab
/// restart — not an escape from containment. Against that it has a real caller
/// (the Claude `PreToolUse` shim and the OpenCode plugin) with no IPC path
/// available to it, because it fires from a child process. Two hardenings make
/// the residual honest:
///
/// 1. **The `tab` is validated** against the user's configured AI tabs
///    ([`is_configured_tab`]) — the fix for the registry-growth finding, since
///    an unvalidated body-supplied key is the map's whole key space.
/// 2. **An engagement writes an origin-marked row** ([`outbound::Origin::Http`]),
///    so the feed says a local process asserted this rather than implying the
///    user did. Bounded to one row per tab-session by
///    [`BeaconOutcome::engaged`], because the latch is sticky.
///
/// Answers 200 with the tab's resulting view for every beacon it accepts,
/// including one with no tab identity (nothing engaged, `latch: "open"`). The
/// reporter is a fail-open shim that discards the body; the status code exists
/// for a human reading a trace, not for control flow — which is also why a
/// rejected tab id gets a 400 it will never read.
async fn handle_latch_beacon(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: LatchBeaconBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let agent =
        crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or("claude"));
    // ONE settings read for the whole request: the tab-id check, `latch_scope`
    // and the policy must not resolve against three different snapshots.
    let settings = live_settings(app);
    // #45: reject an unknown tab id explicitly rather than letting it fall into
    // `latch_scope`'s fail-open `None`. The two are the same for the registry
    // (nothing is created either way), but they are not the same for a reader:
    // "this beacon named a tab that does not exist" is a fact worth a log line,
    // and answering 200 to it would tell a prober that the id was accepted.
    //
    // No activity row here, deliberately. The id is entirely caller-supplied,
    // so a row per rejection is an unbounded write into a capped feed — it would
    // evict the genuine rows this issue exists to preserve. The signal's
    // consumer is the enforcement itself (the request is refused) plus the
    // ABSENCE of the engagement row a real beacon leaves.
    //
    // #48: the check reads the SAME resolution the scope does, rather than a
    // second `is_configured_tab` call beside it — two spellings of one rule are
    // two things to keep in step.
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    let tool = bounded_tool(body.tool.as_deref());
    if let LatchScoping::Unknown(tab) = &scoping {
        warn!(
            target: "offload",
            agent,
            tab = %tab,
            tool = %tool,
            "loopback: /latch/beacon rejected — not a configured tab id"
        );
        let r = RunResult {
            ok: false,
            text: None,
            error: Some(format!(
                "unknown tab id {tab:?} — /latch/beacon accepts configured AI tabs only"
            )),
        };
        return write_json(stream, 400, &r).await;
    }
    let scope = scoping.scope();
    let policy = GatePolicy::resolve(&settings, scope);
    // `CallProvenance::http()`: this route is a loopback POST from a local
    // process, and the contamination row it may write has to say so for the
    // same reason the beacon row does — the launch token is readable by
    // anything running as this user (#45).
    let out = latches().beacon(scope, &tool, policy, CallProvenance::http());
    report_beacon(scope, outbound::Origin::Http, &tool, &out);
    write_json(
        stream,
        200,
        &serde_json::json!({ "ok": true, "latch": out.view }),
    )
    .await
}

/// Write the [`Screen::LatchBeacon`](outbound::Screen) row for one beacon, if
/// this beacon is the one that reports.
///
/// Split out of `handle_latch_beacon` (#48, F-3) so the *pair* of rows a beacon
/// produces is assertable: `LatchRegistry::beacon` writes the contamination row
/// itself, this writes the beacon row, and the two say different things — "this
/// conversation stopped being clean" and "a harness-native web tool was
/// detected". A test that only saw one of them could not tell a regression that
/// dropped the other from a design that never had it.
///
/// Keyed on [`BeaconOutcome::report`], not on `engaged`: a beacon that
/// contaminates a `Local`-latched tab moves no latch and used to leave no trace
/// at all, while quarantining every `context_note` the tab made afterwards.
fn report_beacon(
    scope: Option<&LatchScope>,
    origin: outbound::Origin,
    tool: &str,
    out: &BeaconOutcome,
) {
    if !out.report {
        return;
    }
    let Some(scope) = scope else { return };
    let row = beacon_row(origin, tool, out);
    outbound::record_flag(outbound::Flag {
        screen: row.screen,
        origin: row.origin,
        consumer: scope.agent,
        scope: &scope.label(),
        // The scope is in hand — see `LatchScope::attribution`.
        attribution: scope.attribution(),
        session: scope.session.as_deref(),
        tool: &row.tool,
        host: None,
        url: None,
        resolved_ip: None,
        canary: false,
        root: scope.root.clone(),
        detail: &row.detail,
    });
}

/// The upper bound on a caller-supplied tool name before it reaches an activity
/// row, a log line or the TTS surface (#48). Long enough for every real
/// harness tool name (`WebFetch`, `websearch`) with room to spare.
const BEACON_TOOL_MAX: usize = 64;

/// `/latch/beacon`'s `tool`, bounded (#48).
///
/// The field is an arbitrary unbounded string from a request body and it lands
/// in the row's `tool` column and its `detail`. Svelte escapes on render and
/// the row is bounded to one per tab-session, so this is not an injection or a
/// flood — what it is is a caller choosing how many bytes of the feed, the
/// `tracing` output and the TTS surface one beacon occupies. Truncated by
/// **chars**, not bytes, so a multi-byte name cannot be cut mid-codepoint.
///
/// Control-sequence hygiene is a separate concern with its own owner (Phase D,
/// at the surfaces that render); this only bounds length.
fn bounded_tool(raw: Option<&str>) -> String {
    let raw = raw.map(str::trim).filter(|t| !t.is_empty());
    let Some(raw) = raw else {
        return "(native web tool)".to_string();
    };
    bounded_id(raw)
}

/// One caller-supplied identifier, bounded before it reaches an activity row —
/// the truncation half of [`bounded_tool`], shared rather than re-spelled.
///
/// Its second caller is [`record_discovery_skipped`]'s `Unrecognized` arm (#48
/// F-32): a tab id that names no configured tab is an arbitrary unbounded string
/// from a request body, and putting it in a row verbatim would let a caller
/// choose how many bytes of a capped feed one report occupies. **Only ever
/// applied AFTER classification** — truncating first could fold a long invented
/// id onto a configured one, which would turn a bound into a forgery primitive.
///
/// Its third and fourth callers are #48 F-39 and F-37 (locked decision 42), the
/// same string half of the same class: [`LatchScoping::attribution`]'s
/// `Unrecognized` arm — reached by `/graph_run` and `/mcp/call`, and likewise
/// only after [`latch_scope`] classified the full id — and
/// [`contract_drift_row`], where the shim name and the session id a hook shim
/// reports are both arbitrary strings that reach a row.
///
/// Truncated by **chars**, not bytes, so a multi-byte id cannot be cut
/// mid-codepoint. Control-sequence hygiene is a separate concern with its own
/// owner (Phase D, at the surfaces that render); this only bounds length.
fn bounded_id(raw: &str) -> String {
    let mut out: String = raw.chars().take(BEACON_TOOL_MAX).collect();
    if raw.chars().nth(BEACON_TOOL_MAX).is_some() {
        out.push('…');
    }
    out
}

/// A beacon's `injection_flag` row (#45), composed from the origin the caller
/// states rather than one baked in here (#48).
///
/// Pure, so what an incident reader is told is assertable without an
/// `AppHandle` — the same seam [`override_row`] exists for. The text states the
/// origin limit in words rather than leaving it to the `origin` key, because
/// the person reading this after the fact needs to know that "the expected shim
/// sent it" is an assumption, not a finding.
///
/// The first sentence follows the outcome rather than asserting the engagement
/// case (#48): a beacon that contaminates a `Local`-latched tab, or one that
/// arrives with the latch feature off, moves no latch and refuses nothing —
/// saying it did would be the row lying about the one fact it exists to record.
fn beacon_row(origin: outbound::Origin, tool: &str, out: &BeaconOutcome) -> FlagRow {
    let what = if out.engaged {
        "so this tab is now EXTERNAL-latched and its proxied local-capability tools will refuse"
    } else {
        "and this conversation is now CONTAMINATED — the taint latch did not move (it is not \
         Open, or the latch control is off), so nothing is refused, but every memory write from \
         here on is quarantined and every external result keeps its envelope"
    };
    FlagRow {
        screen: outbound::Screen::LatchBeacon,
        origin,
        tool: tool.to_string(),
        detail: format!(
            "NATIVE-WEB BEACON ({tool}, origin: {}): the harness's own web tool is about to run, \
             {what} (latch={}, contaminated={}). This row records an authenticated POST to \
             /latch/beacon from a local process — the cImp beacon shim is the expected sender, \
             but the launch token is readable by anything running as this user, so this is NOT \
             evidence of a user action. This route only ever TIGHTENS: it cannot unlatch and it \
             cannot clear the contamination flag. Clearing that is a user action in cImp's own UI \
             (step 4), and no HTTP route reaches it.",
            origin.as_str(),
            out.view.latch,
            out.view.contaminated,
        ),
    }
}

/// V32 Phase H (locked decision 17): whether the OpenCode native-tool gate is
/// **in force** for one tab — the single resolved boolean the plugin is told, so
/// no part of the three-level hierarchy has to be reimplemented in JS.
///
/// It is the AND of two features, and the second one is the point:
///
/// - [`Feature::OpencodeNativeGate`] — the Phase H switch itself (default off).
/// - [`Feature::TaintLatch`] — because this gate enforces *the latch's*
///   boundary on tools cImp does not route. With the latch feature off the
///   registry stops engaging (see [`GatePolicy`]), so the latch label the plugin
///   would read is not a boundary anyone is maintaining; denying against it
///   would be enforcement without a policy behind it.
///
/// Resolving it here rather than in the plugin is also what keeps the taint
/// latch a LIVE feature: the gate's own flag is spawn-baked into the plugin
/// file, but this AND is recomputed on every query, so switching the latch off
/// stops the denials without a tab restart.
/// Takes the resolved injection scope rather than a [`LatchScope`] (#48), so
/// the app-wide answer — the one a call with no *usable* tab identity gets — is
/// expressible. See [`LatchScoping::injection`].
fn native_gate_verdict(
    settings: &crate::settings::Settings,
    s: crate::settings::injection::Scope<'_>,
) -> bool {
    use crate::settings::injection::{effective, Feature};
    effective(Feature::OpencodeNativeGate, s, settings)
        && effective(Feature::TaintLatch, s, settings)
}

/// `POST /latch/state`: the resolved containment state of one tab (V32 Phase H).
///
/// **Why a new route rather than an extension of an existing one.**
/// `/latch/beacon` *mutates* — it engages the EXTERNAL latch — so reusing it for
/// a read would latch a tab every time the model touched a local file.
/// `/status` is the whole-app debug view (every tab, every feature, at every
/// scope): far more than a hook on the hot path should parse, and it answers
/// nothing about *this* tab without the plugin knowing which row is its own.
/// This route answers exactly the two facts the gate needs, for one tab.
///
/// Behind the same bearer check as every other route (it precedes dispatch in
/// [`handle_conn`]), because the reply describes a tab's containment posture.
///
/// **Always 200, and always fail-open in shape**: a tab the proxy has never
/// served, or a body with no identity at all, answer `latch: "open"` — the
/// value that denies nothing. The plugin's own error paths land on the same
/// verdict, so "the app is down" and "the app says no gate" are the same
/// behaviour rather than two.
///
/// **The `gate` half is NOT hard-coded off for an unusable tab id (#48).** It
/// resolves the feature hierarchy at whatever scope the body earns — the tab's
/// own when the id is configured, app-wide otherwise. An id that names no
/// configured tab is very often a real tab that was removed or re-id'd while
/// its per-*directory* OpenCode plugin file kept the old id (the unfixed H-2),
/// and "the gate is switched off" and "the gate cannot find your tab" must not
/// be the same answer.
///
/// Known residual, stated rather than papered over: because an unknown id keys
/// no registry entry (#45's bound, deliberately kept), its `latch` is always
/// `open`, and the plugin denies only on `external`/`local`. So the practical
/// effect for a stale plugin file is still "nothing is refused" — what changes
/// is that the verdict now reflects a decision someone took instead of a
/// collapsed `Option`. Closing it properly needs H-2: a per-tab plugin file.
async fn handle_latch_state(
    stream: &mut TcpStream,
    app: &AppHandle,
    req: &Request,
) -> AppResult<()> {
    let body: LatchStateBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            let r = RunResult {
                ok: false,
                text: None,
                error: Some(format!("bad request body: {e}")),
            };
            return write_json(stream, 400, &r).await;
        }
    };
    let agent = crate::graph::source_for_consumer(body.consumer.as_deref().unwrap_or("opencode"));
    let settings = live_settings(app);
    let scoping = latch_scope(app, &settings, agent, body.tab.as_deref());
    // #48: the verdict comes from the resolved injection scope, which is
    // app-wide for both identity-less cases — NOT a hard `false`. #45 folded
    // "an id that names no configured tab" into `latch_scope`'s `None`, and
    // this arm read that `None` as "off", so a stale plugin file (see
    // `LatchScoping::Unknown`) turned the Phase H gate off with only a `warn!`
    // on the sibling beacon route to say so. The registry is untouched either
    // way: `view_for` needs a real scope and never creates one.
    let view = scoping
        .scope()
        .map_or_else(LatchView::default, |scope| latches().view_for(scope));
    write_json(stream, 200, &latch_state_reply(&settings, &scoping, view)).await
}

/// `POST /latch/state`'s reply body, given a resolved scoping and this tab's
/// view.
///
/// Split out of [`handle_latch_state`] (#48) because the regression this issue
/// fixes lived in a `match` arm *here* — `None => (false, …)` — and this crate
/// has no `tauri::test` `AppHandle` mock, so the handler itself is unreachable
/// from a test. Everything that decides the reply is in this function now; the
/// handler's remaining work is the registry lookup, which needs the process
/// global. Re-adding "an unusable tab id means the gate is off" therefore means
/// writing it where `an_unknown_tab_id_resolves_the_app_wide_gate_verdict_not_a_hard_off`
/// can see it.
fn latch_state_reply(
    settings: &crate::settings::Settings,
    scoping: &LatchScoping,
    view: LatchView,
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        // The RESOLVED verdict, not the stored switch: the plugin holds no
        // part of the hierarchy. Deliberately NOT branched on whether the
        // scoping named a usable tab — see `LatchScoping::injection`.
        "gate": native_gate_verdict(settings, scoping.injection()),
        // Flattened rather than nested so the hook reads one string. The
        // full view rides along for a human reading a trace.
        "latch": view.latch,
        "contaminated": view.contaminated,
        // #48 (F-23): WHY the latch is where it is, for the one position that has
        // two possible causes. The plugin refuses the same calls either way — this
        // decides only which fixed refusal it serves, so a plugin (or a loopback)
        // that does not know the field loses nothing but the better message.
        "local_by_user_flip": view.local_by_user_flip,
    })
}

/// `GET /status`: the proxy's V32 Phase B debug view — one row per tab it has
/// served, with that tab's resolved session and latch state
/// ([`Latch::label`]). Read by hand (and by the live-verification recipes) to
/// answer "why is this tab being refused?" without turning on trace logging.
///
/// Behind the same bearer token as every other route; it exposes no fetched
/// content, only cImp's own identifiers and three fixed labels.
/// V32 Phase G (locked decision 16) adds the `injection` object: the RESOLVED
/// value of every control at every scope, and which of the three levels decided
/// it. With three levels, "why is this tab not latching?" has to be answerable
/// without reading code — and `/status` is where the live-verification recipes
/// already look.
async fn handle_status(stream: &mut TcpStream, app: &AppHandle) -> AppResult<()> {
    write_json(
        stream,
        200,
        &serde_json::json!({
            // Step 4: through `latch_snapshot`, so a hand-run `/status` and the
            // UI's badge poll see the same freshness rule rather than two.
            "latches": latch_snapshot(app),
            "injection": injection_status(&live_settings(app)),
        }),
    )
    .await
}

/// The `/status` + `latch_status` introspection view of the enable hierarchy:
/// the master switch, whether protection is reduced anywhere, and one row per
/// scope naming every feature's resolved value and deciding level.
///
/// Scopes reported, always all of them: `app` (the app-wide controls), the
/// `offload-worker` pseudo-scope, and every configured AI tab. Reporting a scope
/// even when nothing is overridden there is the point — the question this
/// answers is "what is in force", and an absent row reads as "off" to exactly
/// the user who is trying to find out.
///
/// **#48 F-35 — the `app` row reports [`Scope::UnknownCaller`], not
/// [`Scope::AppWide`], and that is deliberate for now.** Locked decision 36
/// split one `Scope::App` into those two; this row's label says
/// *"Application-wide"* while its numbers are the identity-less caller's — the
/// app-wide baseline PLUS any configured tab's L3 `On` (N-1). That mismatch is
/// pre-existing rather than introduced here: it is exactly what the row has
/// always published, it is what live-verify recipe *"an identity-less call
/// honours a per-tab `On`"* observes through `/status` (the app row reading
/// `decided_by:"scope"` while its own `override_value` is `"inherit"`), and
/// moving it to `AppWide` would change `GET /status` JSON and take that recipe's
/// only observation point away. Repointing it is a behaviour change with its own
/// retest box and is raised as **F-38**, not folded into the split.
///
/// [`Scope::UnknownCaller`]: crate::settings::injection::Scope::UnknownCaller
/// [`Scope::AppWide`]: crate::settings::injection::Scope::AppWide
pub fn injection_status(settings: &crate::settings::Settings) -> serde_json::Value {
    use crate::settings::injection::{self as inj, Scope};
    let mut scopes = vec![
        serde_json::json!({
            "scope": Scope::UnknownCaller.key(),
            "label": "Application-wide",
            "features": inj::report(settings, Scope::UnknownCaller),
        }),
        serde_json::json!({
            "scope": Scope::OffloadWorker.key(),
            "label": "Offload worker",
            "features": inj::report(settings, Scope::OffloadWorker),
        }),
    ];
    for t in &settings.tabs {
        if let crate::settings::TabConfig::AiTool(c) = t {
            let scope = Scope::tab_only(&c.id);
            scopes.push(serde_json::json!({
                "scope": c.id,
                "label": c.name,
                "features": inj::report(settings, scope),
            }));
        }
    }
    serde_json::json!({
        "protection": inj::master_enabled(settings),
        "reduced": inj::protection_reduced(settings),
        "scopes": scopes,
    })
}

/// Read one `?key=value` query parameter off a request path.
///
/// Deliberately no percent-decoding: every value on these routes is composed by
/// cImp itself (consumer names, tab ids — `[a-z0-9-]`), never by a user or a
/// browser. Matches the pre-V30 behaviour of [`consumer_of`], which this now
/// backs.
fn query_param<'a>(path: &'a str, key: &str) -> Option<&'a str> {
    let (_, query) = path.split_once('?')?;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Parse the `?consumer=<name>` query value off a request path into a
/// [`Consumer`]. Absent / unknown ⇒ Claude (the original default).
fn consumer_of(req: &Request) -> Consumer {
    Consumer::parse(query_param(&req.path, "consumer").unwrap_or("claude"))
}

/// Render one `event: push` SSE frame from a [`PushNotice`].
///
/// The frame grammar is the SSE minimum the child's parser understands:
/// `event: push\ndata: <one-line JSON>\n\n`. `serde_json` escapes every control
/// character, so the payload is *guaranteed* to be a single line however
/// multi-line the pushed content is — the one-line invariant the wire format
/// depends on is enforced by the encoder, not by the caller. Pure, so the shape
/// is unit-testable without a socket — including from the child's side of the
/// wire (`mcp::tests`), which pins encoder and decoder against each other.
pub(super) fn push_frame(notice: &PushNotice) -> Vec<u8> {
    let data =
        serde_json::to_string(notice).unwrap_or_else(|_| r#"{"content":"","meta":{}}"#.to_string());
    format!("event: push\ndata: {data}\n\n").into_bytes()
}

/// `GET /events`: an SSE stream carrying two event types to one per-tab
/// `--offload-mcp` child —
///
/// - `event: change` — the pre-V30 capability pulse, sent to EVERY subscriber
///   (semantics unchanged; the child relays it as `tools/list_changed`);
/// - `event: push` — V30 Phase B, sent only to subscribers a push is addressed
///   to, carrying the semantic [`PushNotice`] payload the child wraps into
///   `notifications/claude/channel`.
///
/// Periodic keep-alive comments (every 20 s) keep idle intermediaries — and the
/// child's own 60 s read-idle watchdog — from dropping the connection.
///
/// The subscriber's identity comes from the child's query params
/// (`?tab=&consumer=&channels=`); `channels=1` means the child ACTUALLY declared
/// the capability on its handshake, not that the setting is on. Registration
/// happens after auth (the caller's job) and is undone by
/// [`PushGuard`](super::service::PushGuard)'s `Drop` when this loop exits — for
/// any reason at all.
async fn handle_events(
    mut stream: TcpStream,
    service: Arc<OffloadService>,
    req: &Request,
) -> AppResult<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\n\
                Connection: keep-alive\r\n\r\n";
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("events head: {e}")))?;
    // Prime the stream so the child's reader unblocks immediately.
    let _ = stream.write_all(b": connected\n\n").await;
    let _ = stream.flush().await;

    // V30 Phase B: register this child in the instance's push registry. The
    // guard is bound for the whole loop — dropping it (on ANY exit below, or on
    // task cancellation) is the sole deregistration path.
    let tab = query_param(&req.path, "tab")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let consumer = query_param(&req.path, "consumer")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("claude")
        .to_string();
    // Anything but an explicit affirmative means "no channels" — a pre-V30
    // child sends no `channels` param at all and must never be pushed to.
    let channels = matches!(query_param(&req.path, "channels"), Some("1") | Some("true"));
    debug!(
        tab = ?tab,
        consumer = %consumer,
        channels,
        "offload loopback: /events subscriber connected"
    );
    let (_push_guard, mut push_rx) = service.register_push_subscriber(tab, consumer, channels);

    let mut rx = service.subscribe_changes();
    loop {
        let tick = tokio::time::sleep(Duration::from_secs(20));
        tokio::select! {
            // V30 Phase B: an addressed push for THIS child.
            notice = push_rx.recv() => {
                match notice {
                    Some(n) => {
                        if stream.write_all(&push_frame(&n)).await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Unreachable while `_push_guard` lives (it owns the only
                    // path that removes our sender), but a closed queue can
                    // never yield another notice — stop selecting on it.
                    None => break,
                }
            }
            recv = rx.recv() => {
                match recv {
                    Ok(()) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    // Lagged: still emit one change so the child re-syncs.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if stream.write_all(b"event: change\ndata: {}\n\n").await.is_err() {
                            break;
                        }
                        let _ = stream.flush().await;
                    }
                    Err(_) => break, // sender dropped
                }
            }
            _ = tick => {
                if stream.write_all(b": keep-alive\n\n").await.is_err() {
                    break;
                }
                let _ = stream.flush().await;
            }
        }
    }
    Ok(())
}

/// Write a one-shot response with a JSON-serializable body.
async fn write_json<T: Serialize>(stream: &mut TcpStream, status: u16, body: &T) -> AppResult<()> {
    let json = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    write_simple(stream, status, "application/json; charset=utf-8", &json).await
}

/// Write a one-shot HTTP response and close.
async fn write_simple(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> AppResult<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| AppError::Offload(format!("write head: {e}")))?;
    stream
        .write_all(body)
        .await
        .map_err(|e| AppError::Offload(format!("write body: {e}")))?;
    let _ = stream.flush().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// V32 Phase G: the default posture — both feature switches on. Every
    /// pre-Phase-G latch test asserted this implicitly, so it is the value they
    /// keep asserting; the switched-off behaviour has its own tests below.
    const ON: GatePolicy = GatePolicy {
        latch: true,
        quarantine: true,
    };

    /// The provenance a NATIVE route states (#48, F-3): cImp's own dispatch,
    /// no fetched page in view. What every pre-F-3 `gate` test was implicitly
    /// asserting, since none of them was an intake.
    const NO_CONTENT: CallProvenance<'static> = CallProvenance::internal();

    /// #48 F-16: what `/graph_run` actually states — a native route that knows
    /// which project the call runs against. Any test that drives a
    /// PERSISTENT-WRITE through `gate` must use this rather than [`NO_CONTENT`],
    /// because that path writes a `MemoryQuarantine` row and `record_flag`'s
    /// tripwire refuses to let one be filed under no project.
    const NATIVE_IN_PROJECT: CallProvenance<'static> = CallProvenance::internal_in(TEST_ROOT);
    /// The provenance the `/latch/beacon` route states — always `Http`.
    const BEACON_PROV: CallProvenance<'static> = CallProvenance::http();

    /// The project root the test scopes claim. A real scope's root is resolved
    /// from the tab's settings entry (`tab_root_key`); the tests care only that
    /// it is carried through to the row, so one fixed value keeps the
    /// assertions readable.
    const TEST_ROOT: &str = "P:\\proj";

    #[test]
    fn find_subslice_locates_header_end() {
        let hay = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nbody";
        let pos = find_subslice(hay, b"\r\n\r\n").unwrap();
        assert_eq!(&hay[pos..pos + 4], b"\r\n\r\n");
    }

    #[test]
    fn authorized_requires_exact_bearer() {
        let req = Request {
            method: "POST".into(),
            path: "/run".into(),
            auth: Some("Bearer abc123".into()),
            body: Vec::new(),
        };
        assert!(authorized(&req, "abc123"));
        assert!(!authorized(&req, "nope"));
        let none = Request {
            method: "GET".into(),
            path: "/describe".into(),
            auth: None,
            body: Vec::new(),
        };
        assert!(!authorized(&none, "abc123"));
    }

    // ── V30 Phase B: /events subscriber identity + the push frame ─────────

    #[test]
    fn query_param_reads_the_events_identity() {
        let path = "/events?tab=claude-2&consumer=claude&channels=1";
        assert_eq!(query_param(path, "tab"), Some("claude-2"));
        assert_eq!(query_param(path, "consumer"), Some("claude"));
        assert_eq!(query_param(path, "channels"), Some("1"));
        assert_eq!(query_param(path, "nope"), None);
        // A prefix must not match a different key, and a bare path has none.
        assert_eq!(query_param("/events?consumer=opencode", "consume"), None);
        assert_eq!(query_param("/events", "tab"), None);
        // The pre-V30 child sends no query at all: it must still parse as the
        // default consumer, with no tab and no channels.
        let legacy = Request {
            method: "GET".into(),
            path: "/events".into(),
            auth: None,
            body: Vec::new(),
        };
        assert_eq!(consumer_of(&legacy), Consumer::Claude);
        assert!(!matches!(
            query_param(&legacy.path, "channels"),
            Some("1") | Some("true")
        ));
    }

    /// The wire contract the child's SSE parser depends on: one `event:` line,
    /// one single-line `data:` line, blank-line terminated — even when the
    /// pushed content itself contains newlines (serde escapes them).
    #[test]
    fn push_frame_is_a_single_line_sse_data_payload() {
        let notice = PushNotice::new(
            "line one\nline two\r\nline three",
            &[],
            [("kind", "audit_done"), ("seq", "3")],
        );
        let frame = String::from_utf8(push_frame(&notice)).unwrap();
        let lines: Vec<&str> = frame.split('\n').collect();
        assert_eq!(lines[0], "event: push");
        assert!(lines[1].starts_with("data: "));
        // event / data / "" / "" — exactly one data line, blank-line terminated.
        assert_eq!(lines.len(), 4, "frame was: {frame:?}");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "");
        let round: PushNotice = serde_json::from_str(&lines[1]["data: ".len()..]).unwrap();
        assert_eq!(round, notice);
    }

    #[test]
    fn token_is_long_and_random() {
        let a = make_token();
        let b = make_token();
        assert_ne!(a, b);
        assert!(a.len() >= 32);
    }

    // ── NC-2: permission-hook classification + tab mapping ─────────────────

    #[test]
    fn permission_denied_resolves_and_unknown_events_are_ignored() {
        assert_eq!(
            classify_permission_event("PermissionDenied", "", ""),
            Some(PermissionEdge::Resolved)
        );
        // Events we never registered (and the PermissionRequest we chose NOT to
        // adopt) must not move the flag even if one somehow reaches the route.
        for event in ["PermissionRequest", "PreToolUse", "", "Stop"] {
            assert_eq!(
                classify_permission_event(event, "permission_prompt", ""),
                None
            );
        }
    }

    #[test]
    fn notification_type_drives_classification_when_present() {
        assert_eq!(
            classify_permission_event("Notification", "permission_prompt", ""),
            Some(PermissionEdge::Detected)
        );
        // Case/whitespace tolerance — the value is echoed from the payload.
        assert_eq!(
            classify_permission_event("Notification", " Permission_Prompt ", ""),
            Some(PermissionEdge::Detected)
        );
        // Every other RECOGNIZED type is ignored, including idle (which is NOT
        // wired to the question pipe — see the classifier's doc comment) …
        for kind in IGNORED_NOTIFICATION_TYPES {
            assert_eq!(classify_permission_event("Notification", kind, ""), None);
        }
        // … and a recognized type WINS over the prose fallback, so a future
        // permission-flavoured message under a non-permission type can't leak
        // in. Deliberate and pinned: idle notifications stay out of the
        // permission pipe whatever their wording says.
        assert_eq!(
            classify_permission_event(
                "Notification",
                "idle_prompt",
                "Claude needs your permission to use Bash"
            ),
            None
        );
        assert_eq!(
            classify_permission_event(
                "Notification",
                "Agent_Completed",
                "Claude needs your permission to use Bash"
            ),
            None,
            "recognized types are matched case-insensitively too"
        );
    }

    /// M12 (2026-08-05 review): an UNRECOGNIZED non-empty type must fall
    /// through to the prose check instead of short-circuiting to "ignored".
    /// The `Notification` payload shape is explicitly UNVERIFIED
    /// (`notify_hook.rs` module doc), so a renamed type — or a nested field the
    /// shim reads into `notification_type` — is the expected drift, and for the
    /// permission case "ignored" is silence: the badge/TTS never fire and
    /// nothing logs above debug.
    #[test]
    fn unrecognized_notification_type_falls_through_to_the_prose_check() {
        // Renamed/unknown type + permission prose ⇒ still detected.
        for kind in ["tool_permission", "permission-prompt", "something_new"] {
            assert_eq!(
                classify_permission_event(
                    "Notification",
                    kind,
                    "Claude needs your permission to use Bash"
                ),
                Some(PermissionEdge::Detected),
                "{kind}"
            );
        }
        // Unknown type + unrelated prose ⇒ ignored, as before.
        for msg in [
            "Claude is waiting for your input",
            "Error: permission denied while reading /etc/shadow",
            "",
        ] {
            assert_eq!(
                classify_permission_event("Notification", "something_new", msg),
                None,
                "{msg}"
            );
        }
    }

    #[test]
    fn notification_message_classifies_when_type_field_is_absent() {
        assert_eq!(
            classify_permission_event(
                "Notification",
                "",
                "Claude needs your permission to use Bash"
            ),
            Some(PermissionEdge::Detected)
        );
        assert_eq!(
            classify_permission_event("Notification", "", "Permission to use Edit is required"),
            Some(PermissionEdge::Detected)
        );
        // Idle prose, and a "permission denied" error that must NOT be read as
        // a prompt (why the marker is narrower than a bare "permission").
        for msg in [
            "Claude is waiting for your input",
            "Error: permission denied while reading /etc/shadow",
            "",
        ] {
            assert_eq!(
                classify_permission_event("Notification", "", msg),
                None,
                "{msg}"
            );
        }
    }

    fn cand(tab: &str, session: Option<&str>, cwd: &str) -> PermissionTabCandidate {
        PermissionTabCandidate {
            tab: tab.to_string(),
            session_id: session.map(str::to_string),
            cwd: PathBuf::from(cwd),
        }
    }

    #[test]
    fn tab_mapping_prefers_session_id_then_transcript_then_unique_cwd() {
        let tabs = [
            cand("claude", Some("sess-a"), "C:/proj"),
            cand("ai-2", Some("sess-b"), "C:/proj/wt"),
            cand("claude-local", None, "C:/proj"),
        ];
        // 1. session id.
        assert_eq!(
            resolve_permission_tab(&tabs, "sess-b", "", ""),
            Some("ai-2".to_string())
        );
        // 2. transcript stem, when the session field went missing.
        assert_eq!(
            resolve_permission_tab(
                &tabs,
                "",
                "C:/Users/x/.claude/projects/slug/sess-a.jsonl",
                ""
            ),
            Some("claude".to_string())
        );
        // 3. cwd, but only where it names exactly one tab: `C:/proj` is shared
        // by two tabs (ambiguous ⇒ drop), the worktree dir is unique.
        assert_eq!(resolve_permission_tab(&tabs, "", "", "C:/proj"), None);
        assert_eq!(
            resolve_permission_tab(&tabs, "", "", "C:/proj/wt"),
            Some("ai-2".to_string())
        );
        // Separator/trailing-slash normalization (and, on Windows, case).
        assert_eq!(
            resolve_permission_tab(&tabs, "", "", "C:\\proj\\wt\\"),
            Some("ai-2".to_string())
        );
        // Nothing matches ⇒ dropped, never guessed.
        assert_eq!(
            resolve_permission_tab(&tabs, "sess-zz", "", "D:/elsewhere"),
            None
        );
        assert_eq!(resolve_permission_tab(&tabs, "", "", ""), None);
        assert_eq!(resolve_permission_tab(&[], "sess-a", "", "C:/proj"), None);
    }

    #[test]
    fn tab_mapping_refuses_a_session_claimed_by_two_tabs() {
        let tabs = [
            cand("claude", Some("dup"), "C:/a"),
            cand("claude-local", Some("dup"), "C:/b"),
        ];
        assert_eq!(resolve_permission_tab(&tabs, "dup", "", ""), None);
    }

    /// H1 (2026-08-05 review): two RUNNING Claude tabs on one project make every
    /// tab-keyed identity claim unprovable, so `live_claude_sessions` (the sole
    /// source of `session_id` here) hands both candidates `None` — see
    /// `graph::service::live_claude_tab_sessions`. This pins the resulting
    /// contract on THIS side of the seam: refuse, never guess.
    #[test]
    fn tab_mapping_refuses_when_the_registry_withholds_ambiguous_bindings() {
        // Both same-root tabs, session bindings withheld at the registry.
        let tabs = [
            cand("claude", None, "C:/proj"),
            cand("claude-local", None, "C:/proj"),
        ];
        // The hook payload names a real live session — but nothing claims it.
        assert_eq!(resolve_permission_tab(&tabs, "sess-b", "", "C:/proj"), None);
        assert_eq!(
            resolve_permission_tab(
                &tabs,
                "",
                "C:/Users/x/.claude/projects/slug/sess-b.jsonl",
                "C:/proj"
            ),
            None
        );
        // ...and the cwd fallback declines too: the shared root is, by
        // construction, shared by ≥2 tabs.
        assert_eq!(resolve_permission_tab(&tabs, "", "", "C:/proj"), None);
        // A single running tab per root keeps its binding and still resolves.
        let solo = [
            cand("claude", Some("sess-a"), "C:/proj"),
            cand("ai-2", None, "C:/other"),
        ];
        assert_eq!(
            resolve_permission_tab(&solo, "sess-a", "", ""),
            Some("claude".to_string())
        );
    }

    #[test]
    fn permission_event_body_tolerates_a_minimal_payload() {
        // Only the event name — every other field defaults, so a drifted
        // payload still deserializes and is simply unmappable (dropped).
        let body: PermissionEventBody =
            serde_json::from_value(serde_json::json!({ "event": "Notification" }))
                .expect("minimal body deserializes");
        assert_eq!(body.event, "Notification");
        assert!(body.session_id.is_none() && body.cwd.is_none());
    }

    // ── V24 Phase F: OpenCode usage-event arm ──────────────────────────────

    fn usage_body(json: serde_json::Value) -> MemoryEventBody {
        serde_json::from_value(json).expect("usage body deserializes")
    }

    #[test]
    fn usage_body_well_formed_records_session_turn() {
        // No parent → recorded against the reporting session with origin Session.
        let body = usage_body(serde_json::json!({
            "cwd": ".", "agent": "opencode", "kind": "usage",
            "session_id": "ses_main", "msg_id": "msg_1", "model": "qwen3-coder",
            "in_tok": 100, "out_tok": 40, "cache_read": 20, "cache_make": 5,
        }));
        let (target, event) =
            usage_event_from_body(&body).expect("well-formed body yields an event");
        assert_eq!(target, "ses_main");
        match &event {
            crate::graph::UsageEvent::Turn {
                msg_id,
                model,
                in_tok,
                out_tok,
                cache_read,
                cache_make,
                origin,
            } => {
                assert_eq!(msg_id, "msg_1");
                assert_eq!(model.as_deref(), Some("qwen3-coder"));
                assert_eq!(
                    (*in_tok, *out_tok, *cache_read, *cache_make),
                    (100, 40, 20, 5)
                );
                assert_eq!(*origin, crate::graph::UsageOrigin::Session);
            }
            _ => panic!("expected a Turn event"),
        }

        // Recording it lands a real turn row (est_only clears).
        let dir = std::env::temp_dir().join(format!("cimp-usage-sess-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(&target, "opencode", &event, 100)
            .unwrap();
        let series = idx.usage_turn_series("ses_main").unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].msg_id, "msg_1");
        assert_eq!(series[0].origin, crate::graph::UsageOrigin::Session);
        assert_eq!(series[0].in_tok, 100);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_body_with_parent_rolls_up_as_agent() {
        // A child (sub-agent) session's spend is attributed to the PARENT with
        // origin Agent — mirrors the Claude sub-agent contract.
        let body = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "ses_child", "parent_session_id": "ses_parent",
            "msg_id": "msg_a", "model": "qwen3-coder",
            "in_tok": 7, "out_tok": 3, "cache_read": 0, "cache_make": 0,
        }));
        let (target, event) = usage_event_from_body(&body).expect("child body yields an event");
        assert_eq!(target, "ses_parent", "spend rolls up to the parent");
        match &event {
            crate::graph::UsageEvent::Turn { origin, .. } => {
                assert_eq!(*origin, crate::graph::UsageOrigin::Agent);
            }
            _ => panic!("expected a Turn event"),
        }

        let dir = std::env::temp_dir().join(format!("cimp-usage-parent-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(&target, "opencode", &event, 100)
            .unwrap();
        // The turn lives on the parent, not the child.
        assert_eq!(idx.usage_turn_series("ses_parent").unwrap().len(), 1);
        assert!(idx.usage_turn_series("ses_child").unwrap().is_empty());
        let series = idx.usage_turn_series("ses_parent").unwrap();
        assert_eq!(series[0].origin, crate::graph::UsageOrigin::Agent);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_body_malformed_or_empty_is_ignored() {
        // Missing msg_id → no event.
        let no_msg = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "in_tok": 10,
        }));
        assert!(usage_event_from_body(&no_msg).is_none());
        // Empty msg_id → no event.
        let empty_msg = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "msg_id": "", "in_tok": 10,
        }));
        assert!(usage_event_from_body(&empty_msg).is_none());
        // All-zero token totals (degenerate/creation emit) → skipped.
        let all_zero = usage_body(serde_json::json!({
            "kind": "usage", "session_id": "s", "msg_id": "m",
            "in_tok": 0, "out_tok": 0, "cache_read": 0, "cache_make": 0,
        }));
        assert!(usage_event_from_body(&all_zero).is_none());
    }

    #[test]
    fn usage_upsert_by_msg_id_does_not_duplicate() {
        // The plugin emits the final turn twice (spike-confirmed) — same msg_id,
        // so the second overwrites the first in place rather than appending.
        let mk = |out: u64| {
            usage_body(serde_json::json!({
                "kind": "usage", "session_id": "ses", "msg_id": "dup",
                "in_tok": 50, "out_tok": out, "cache_read": 0, "cache_make": 0,
            }))
        };
        let dir = std::env::temp_dir().join(format!("cimp-usage-dup-{}", uuid::Uuid::new_v4()));
        let idx = crate::graph::GraphIndex::open(&dir, ".ckg").expect("open");
        for out in [10u64, 20u64] {
            let (target, event) = usage_event_from_body(&mk(out)).expect("event");
            idx.record_usage_event(&target, "opencode", &event, 100)
                .unwrap();
        }
        let series = idx.usage_turn_series("ses").unwrap();
        assert_eq!(series.len(), 1, "duplicate msg_id upserts, not appends");
        assert_eq!(series[0].out_tok, 20, "last emit wins");
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_event_parent_flags_child_sessions_only() {
        // V24 code-review: a tool event with no parent is the reporting session's
        // own event (recorded normally); one carrying a non-empty parent is a
        // task-tool child whose events are dropped and rolled up to the parent.
        let own = usage_body(serde_json::json!({
            "session_id": "s", "tool": "read", "args": {}
        }));
        assert_eq!(
            tool_event_parent(&own),
            None,
            "no parent field → own session"
        );
        let empty_parent = usage_body(serde_json::json!({
            "session_id": "s", "tool": "read", "args": {}, "parent_session_id": ""
        }));
        assert_eq!(
            tool_event_parent(&empty_parent),
            None,
            "empty parent → own session"
        );
        let child = usage_body(serde_json::json!({
            "session_id": "ses_child", "tool": "read", "args": {}, "parent_session_id": "ses_parent"
        }));
        assert_eq!(
            tool_event_parent(&child),
            Some("ses_parent".to_string()),
            "child → parent"
        );
    }

    #[test]
    fn discovery_round_trips() {
        let d = Discovery {
            port: 8123,
            token: "tok".into(),
            pid: 42,
            root: "P:\\proj".into(),
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: Discovery = serde_json::from_str(&s).unwrap();
        assert_eq!(back.port, 8123);
        assert_eq!(back.token, "tok");
        assert_eq!(back.pid, 42);
        // Legacy files (pre-root) still parse: `root` defaults empty.
        let legacy: Discovery = serde_json::from_str(r#"{"port":1,"token":"t","pid":9}"#).unwrap();
        assert_eq!(legacy.root, "");
    }

    fn disc(pid: u32, port: u16, root: &str) -> Discovery {
        Discovery {
            port,
            token: format!("tok{pid}"),
            pid,
            root: root.to_string(),
        }
    }

    /// A project root or child cwd, spelled the way the host platform spells
    /// one: `P:\<tail>` on Windows — byte-identical to the literals these
    /// fixtures used to hard-code — and `/<tail>` everywhere else.
    ///
    /// **Why this cannot be a hard-coded literal.** Discovery routing is
    /// component-wise ([`is_ancestor_or_equal`]), and `Path::new(r"P:\proj\b")`
    /// is a SINGLE `Component::Normal` on Linux. Every root/hint pair below
    /// would therefore stop matching, step 1 of [`select_answering`] would rank
    /// nothing, and the tests would either panic on `.expect("match")` or —
    /// worse — pass through the sole-entry fallback while asserting nothing
    /// about the ranking they were written for. The properties being pinned
    /// here (F-11, F-26, F-28, decision 30's probe budget) are not
    /// Windows-specific, so their coverage must not be either.
    fn proj(tail: &str) -> String {
        if cfg!(windows) {
            format!(r"P:\{}", tail.replace('/', "\\"))
        } else {
            format!("/{tail}")
        }
    }

    /// [`proj`] as a `PathBuf`, for the cwd/hint side of the same pairs.
    fn proj_path(tail: &str) -> PathBuf {
        PathBuf::from(proj(tail))
    }

    /// The preference ORDER on its own: every candidate answers and there is no
    /// legacy store.
    ///
    /// These four cases predate locked decision 30 and used to call
    /// `select_discovery`, which now probes real sockets and reads the real
    /// `.cimp-offload.json` next to the test binary. Stubbing liveness all-live
    /// is what keeps them a statement about ORDERING — the property they were
    /// written for — rather than about the machine they run on.
    fn select_all_live(entries: Vec<Discovery>, hint: Option<&Path>) -> Option<Discovery> {
        select_verified(entries, hint, |_| true, || None)
    }

    #[test]
    fn select_discovery_routes_by_root() {
        // Two instances off one install: a child whose cwd is inside project
        // B must reach B's instance, never last-writer-wins.
        let entries = vec![
            disc(1, 1001, &proj("proj/a")),
            disc(2, 1002, &proj("proj/b")),
        ];
        let picked = select_all_live(entries, Some(&proj_path("proj/b/src"))).expect("match");
        assert_eq!(picked.pid, 2);
    }

    #[test]
    fn select_discovery_deepest_matching_root_wins() {
        // Nested checkouts: the closest (deepest) serving instance wins.
        let entries = vec![
            disc(1, 1001, &proj("proj")),
            disc(2, 1002, &proj("proj/nested")),
        ];
        let picked = select_all_live(entries, Some(&proj_path("proj/nested/src"))).expect("match");
        assert_eq!(picked.pid, 2);
        // A hint outside the nested root resolves to the outer instance.
        let entries = vec![
            disc(1, 1001, &proj("proj")),
            disc(2, 1002, &proj("proj/nested")),
        ];
        let picked = select_all_live(entries, Some(&proj_path("proj/other"))).expect("match");
        assert_eq!(picked.pid, 1);
    }

    #[cfg(windows)]
    #[test]
    fn select_discovery_is_case_insensitive_on_windows() {
        let entries = vec![disc(1, 1001, "p:\\PROJ\\A")];
        let picked =
            select_all_live(entries, Some(Path::new("P:\\proj\\a\\deep"))).expect("match");
        assert_eq!(picked.pid, 1);
    }

    #[test]
    fn select_discovery_sole_entry_wins_without_a_root_match() {
        // One running instance is unambiguous even when the hint doesn't
        // land inside its root (e.g. an agent launched outside any project).
        let entries = vec![disc(7, 1007, "P:\\elsewhere")];
        let picked = select_all_live(entries, Some(Path::new("Q:\\other"))).expect("sole entry");
        assert_eq!(picked.pid, 7);
    }

    /// #48 F-26 — the repro two `graph/mcp.rs` comments used to document, pinned
    /// as a test because the wrong version of that sentence produced a false PASS
    /// in live verification (the tester truncated one file, saw a served call, and
    /// recorded "no fallback reachable").
    ///
    /// A truncated per-instance entry is dropped by `read_all_discoveries`'s
    /// `filter_map(… .ok())`, so selection sees it not at all — and that is
    /// exactly step 3's cue: the legacy `.cimp-offload.json` still resolves, the
    /// app is still reached, and nothing goes headless. `ProxyMiss::NoInstance`
    /// needs BOTH stores unusable.
    #[test]
    fn a_corrupt_per_instance_entry_still_resolves_through_the_legacy_file() {
        let hint = proj_path("proj/src");
        let legacy = disc(99, 4444, "");
        // The corrupted `<pid>.json` is simply absent from the entry list.
        let picked = select_verified(vec![], Some(&hint), |_| true, || Some(legacy.clone()))
            .expect("the legacy store still resolves");
        assert_eq!(picked.pid, 99, "step 3 was not consulted");
        assert_eq!(picked.port, 4444);
        // And only when the first two preferences produce nothing: a matching
        // per-instance entry must never be overridden by the legacy file.
        let picked = select_verified(
            vec![disc(1, 1001, &proj("proj"))],
            Some(&hint),
            |_| true,
            || panic!("the legacy store must not be read when a per-instance entry matches"),
        )
        .expect("match");
        assert_eq!(picked.pid, 1);
        // Decision 30 added a second condition to step 3 that F-26's original
        // wording did not have: the legacy entry must ANSWER too. A legacy file
        // naming a dead endpoint is not a resolution — it is the last candidate,
        // and `NoInstance` is the honest answer once it fails.
        assert!(
            select_verified(vec![], Some(&hint), |_| false, || Some(legacy.clone())).is_none(),
            "a dead legacy entry must not resolve (#48 F-11)"
        );
    }

    /// The single-write trigger F-26 named, **re-pointed by locked decision 30**.
    ///
    /// **This test changed meaning.** It was written as
    /// `a_deeper_well_formed_entry_outranks_the_running_instance` and it pinned
    /// F-11's DEFECT on purpose — a deeper well-formed entry outranked the real
    /// instance *whatever its port*, so ONE `Write` steered a child onto a dead
    /// endpoint (and chose `ProxyMiss::Transport` as the reason the system would
    /// report). Its author left the defect pinned so that a green suite could not
    /// be mistaken for F-11 being closed. Decision 30 closed it, so the assertion
    /// is now the post-fix invariant, and the name says which half is which:
    ///
    /// * a deeper entry that **answers** legitimately still wins — the
    ///   deepest-root preference is deliberately kept (dropping it reintroduces
    ///   "project A's child talks to project B's app");
    /// * a deeper entry that does **not** answer no longer wins, and the real
    ///   instance below it does.
    ///
    /// What it still does NOT claim: that a planted entry cannot win. An attacker
    /// who binds the port they wrote answers the probe. Decision 30's accepted
    /// bound is "one write plus a listener", and this test is the pin on the
    /// *write-only* half — see [`responds`].
    #[test]
    fn a_deeper_entry_outranks_the_running_instance_only_while_it_answers() {
        let real = disc(10, 4000, &proj("proj"));
        let planted = disc(11, 1, &proj("proj/sub"));
        let hint = proj_path("proj/sub/deeper");
        let no_legacy = || panic!("a matching per-instance entry answers");

        // Half 1 — a deeper entry that answers is still preferred. Depth, not
        // liveness, is what ranks; liveness only filters.
        let picked = select_verified(
            vec![real.clone(), planted.clone()],
            Some(&hint),
            |_| true,
            no_legacy,
        )
        .expect("a matching entry exists");
        assert_eq!(picked.pid, 11, "the deepest LIVE entry must still win");

        // Half 2 — the finding itself: the deeper entry is dead, so the running
        // instance underneath it serves the call instead of the child going
        // headless. `dead` is keyed on the port, which is the one thing the
        // planted file cannot fake without a listener.
        let picked = select_verified(
            vec![real, planted],
            Some(&hint),
            |d| d.port != 1,
            no_legacy,
        )
        .expect("the shallower live instance resolves");
        assert_eq!(picked.pid, 10, "a dead deeper entry must not win (#48 F-11)");
        assert_eq!(picked.port, 4000);
    }

    /// #48 F-28 — one `Write` no longer disarms decision 14's native-web sensor.
    ///
    /// `taint_beacon::dispatch` resolves its endpoint with `read_discovery_for`
    /// and is **fail-open by design**: no endpoint means no beacon, silently, so a
    /// `WebFetch` stops contaminating the tab. That fail-open is correct and is
    /// deliberately unchanged — the defect was that the resolution it failed open
    /// *from* was steerable by one file write.
    ///
    /// Driven through the beacon's own resolution shape (its cwd is the project
    /// directory Claude spawned it in) against a REAL socket for the live
    /// instance, so the probe itself — not a stubbed closure — is what rejects the
    /// planted entry. F-28 keeps its own live-verification row; this is the unit
    /// pin, not a substitute for it.
    #[test]
    fn a_planted_dead_entry_no_longer_disarms_the_native_web_beacon() {
        let live = fake_instance("tok-live");
        // The planted file: well-formed, a DEEPER root than the running
        // instance's, and a port nothing is listening on.
        let planted = disc(4242, dead_port(), &proj("proj/sub"));
        let real = Discovery {
            port: live,
            token: "tok-live".into(),
            pid: 10,
            root: proj("proj"),
        };
        let cwd = proj_path("proj/sub/pkg");

        let picked = select_verified(vec![real, planted], Some(&cwd), responds, || None)
            .expect("the beacon still finds the running instance");
        assert_eq!(
            picked.port, live,
            "the beacon must reach the live instance, not the planted endpoint"
        );
        assert_eq!(picked.token, "tok-live");
    }

    /// The probe is an authentication check as well as a liveness check: a socket
    /// that answers but does not recognize the entry's token is somebody else's
    /// process, and accepting it would let a planted file borrow a real port.
    #[test]
    fn the_probe_accepts_only_an_endpoint_that_honours_this_entrys_token() {
        let port = fake_instance("tok-right");
        let right = Discovery {
            port,
            token: "tok-right".into(),
            pid: 1,
            root: String::new(),
        };
        let wrong = Discovery {
            token: "tok-wrong".into(),
            ..right.clone()
        };
        assert!(responds(&right), "the real token must answer 200");
        assert!(!responds(&wrong), "a 401 is not an answer");
        assert!(
            !responds(&Discovery {
                port: dead_port(),
                ..right
            }),
            "a dead port is not an answer"
        );
    }

    /// The latency bound is a property, not a comment: a resolution probes at
    /// most [`MAX_DISCOVERY_PROBES`] candidates however many entries exist, so the
    /// worst case a hook shim can be made to pay is bounded by the constant and
    /// not by how many files an attacker wrote.
    #[test]
    fn a_resolution_never_probes_more_than_its_budget() {
        let hint = proj_path("proj/a/b/c/d/e/f/g/h");
        // Twenty matching entries, each deeper than the last, none answering.
        let entries: Vec<Discovery> = (0..20)
            .map(|i| disc(i, 1000 + i as u16, &proj("proj")))
            .collect();
        let probes = std::cell::Cell::new(0usize);
        let picked = select_verified(
            entries,
            Some(&hint),
            |_| {
                probes.set(probes.get() + 1);
                false
            },
            || Some(disc(99, 4444, "")),
        );
        assert!(picked.is_none(), "nothing answered, so nothing resolves");
        assert!(
            probes.get() <= MAX_DISCOVERY_PROBES,
            "probed {} candidates, budget is {MAX_DISCOVERY_PROBES}",
            probes.get()
        );
        // And the ceiling is a non-answer, never a free pass: exhausting the
        // budget must not let an UNVERIFIED entry through.
        assert!(probes.get() > 0, "the budget must actually be spent");
    }

    /// A sole per-instance entry that is dead now falls through to the legacy
    /// file. New with decision 30 and deliberate: a hard-killed instance leaves
    /// its `<pid>.json` behind (removal is graceful-exit only), so "the sole
    /// surviving entry" can be a corpse while `.cimp-offload.json` names a live
    /// instance. Previously that child went headless.
    #[test]
    fn a_dead_sole_entry_falls_through_to_the_legacy_store() {
        let hint = PathBuf::from("Q:\\other");
        let picked = select_verified(
            vec![disc(7, 1007, "P:\\elsewhere")],
            Some(&hint),
            |d| d.pid == 99,
            || Some(disc(99, 4444, "")),
        )
        .expect("the legacy store names a live instance");
        assert_eq!(picked.pid, 99);
    }

    // ── #48 F-32 / locked decision 37 — the child→app discovery report ───────

    /// A ledger local to one test. The production one is process-global and the
    /// suite runs concurrently, so a test that used it would be racing its
    /// neighbours on the shared `(no tab identity)` bucket — which is exactly
    /// the bucket the key-space property is about.
    fn test_ledger() -> HashMap<String, outbound::Doubling> {
        HashMap::new()
    }

    /// The facts a configured tab resolves to, stood in for. The `Cell` records
    /// whether the closure ran at all: an identity-less report must never reach
    /// the app-side resolvers, because there is no tab to resolve.
    fn facts_probe(
        called: &std::cell::Cell<bool>,
    ) -> impl FnOnce(&str, &'static str) -> TabFacts + '_ {
        move |tab, _agent| {
            called.set(true);
            TabFacts {
                root: format!("P:\\proj\\{tab}"),
                session: Some("sess-f32".to_string()),
            }
        }
    }

    fn skipped_body(tab: Option<&str>, skipped: u32) -> DiscoverySkippedBody {
        DiscoverySkippedBody {
            tab: tab.map(str::to_string),
            consumer: None,
            skipped,
        }
    }

    /// **Decision 37's bar, clauses (1), (2) and (5).** A token-holder can cause
    /// a row; it cannot make that row name a tab that is not configured, claim
    /// `Headless` when the truth is unknown, carry a root or a session it chose,
    /// state a count no genuine child could produce, or move anything.
    ///
    /// Asserted **through** `record_discovery_skipped` with `test_rows`
    /// observing the row the producer actually wrote — not by calling
    /// `tab_identity` beside it and comparing to itself, which is the shape that
    /// let three findings survive their fixes here. Deleting the `tab_identity`
    /// call from the producer fails this test.
    #[test]
    fn a_forged_discovery_report_cannot_claim_a_tab_or_choose_what_the_row_says() {
        use crate::activity::Attribution;
        let s = settings_with_tabs(&["f32-real"]);
        let mut ledger = test_ledger();
        let row_for = |ledger: &mut HashMap<String, outbound::Doubling>,
                       body: &DiscoverySkippedBody|
         -> Option<crate::activity::ActivityRecord> {
            outbound::test_rows::reset();
            let called = std::cell::Cell::new(false);
            record_discovery_skipped(&s, body, |k| claim_in(ledger, k), facts_probe(&called));
            let mut rows = outbound::test_rows::drain();
            assert!(rows.len() <= 1, "one report, at most one row");
            let row = rows.pop();
            // The app-side resolvers run for a CONFIGURED tab and for nothing
            // else: there is no tab to resolve a root or a session for.
            if let Some(r) = &row {
                assert_eq!(
                    called.get(),
                    matches!(&r.entry.tab, Attribution::Tab(_)),
                    "the app-side facts were resolved for a non-tab (or not for a tab)"
                );
            }
            row
        };

        // A configured id is the only one that becomes a tab — and its root and
        // session come from the app, not from the wire (there is no wire field
        // for either).
        let real = row_for(&mut ledger, &skipped_body(Some("f32-real"), 1)).expect("a row");
        assert_eq!(
            real.entry.tab,
            Attribution::Tab("f32-real".to_string())
        );
        assert_eq!(real.entry.root, "P:\\proj\\f32-real");
        assert_eq!(real.entry.session.as_deref(), Some("sess-f32"));
        assert_eq!(real.entry.source, "discovery_skipped");
        assert_eq!(real.entry.tool, "discovery");
        assert!(
            real.entry.ok,
            "containment WORKED — a denial-shaped row would say cImp blocked the child"
        );
        assert!(
            real.request.contains("\"origin\": \"http\""),
            "a local process asserted this, and the row must say so: {}",
            real.request
        );

        // An id naming no configured tab is `Unrecognized`, never `Tab` and
        // never `Headless`, and it carries no root and no session.
        let forged = row_for(&mut ledger, &skipped_body(Some("f32-not-a-tab"), 1)).expect("a row");
        assert_eq!(
            forged.entry.tab,
            Attribution::Unrecognized("f32-not-a-tab".to_string())
        );
        assert!(
            forged.entry.root.is_empty() && forged.entry.session.is_none(),
            "a forged id must not be able to file a security row under a project"
        );

        // No id at all is `Unattributed` — "this writer does not know" — and
        // explicitly NOT `Headless`, which is the positive claim "a worker run
        // with no tab behind it". That collapse is F-20's defect and F-29's; a
        // body-supplied tab is indistinguishable from an invented one, so this
        // frame cannot make the positive claim.
        let anon = row_for(&mut ledger, &skipped_body(None, 1)).expect("a row");
        assert_eq!(anon.entry.tab, Attribution::Unattributed);
        assert_ne!(anon.entry.tab, Attribution::Headless);

        // An unbounded invented id cannot choose how many bytes of a capped feed
        // one report occupies — bounded AFTER classification, so truncation can
        // never fold a long id onto a configured one.
        // (Its own ledger: the sentinel bucket is shared — which is the point of
        // the key-space assertion below — so in the main one this report would
        // land between powers of two and be correctly suppressed.)
        let long = "x".repeat(4096);
        let big = row_for(&mut test_ledger(), &skipped_body(Some(&long), 1)).expect("a row");
        let Attribution::Unrecognized(id) = &big.entry.tab else {
            panic!("a 4096-char id is not a tab: {:?}", big.entry.tab);
        };
        assert!(id.chars().count() <= BEACON_TOOL_MAX + 1, "{}", id.len());
        assert!(
            !is_configured_tab(&s, "claude", id),
            "truncation is not a forgery"
        );

        // The count is caller-asserted and the row says so, clamped to the probe
        // budget a genuine resolution cannot exceed.
        let huge = row_for(&mut ledger, &skipped_body(Some("f32-real"), 9999)).expect("a row");
        assert!(huge.response.contains("skipped 6 candidate"), "{}", huge.response);
        assert!(huge.response.contains("clamped"), "{}", huge.response);
        assert!(
            huge.response.contains("CALLER-ASSERTED"),
            "the honesty clause is not optional: {}",
            huge.response
        );

        // A report of zero skips is not a report — and it is what a malformed or
        // empty body degrades to, so neither writes anything.
        assert!(row_for(&mut ledger, &skipped_body(Some("f32-real"), 0)).is_none());
        for raw in [
            &b"{ not json"[..],
            &b""[..],
            &b"null"[..],
            &br#"{"skipped":"lots"}"#[..],
        ] {
            let body: DiscoverySkippedBody = serde_json::from_slice(raw).unwrap_or_default();
            assert_eq!(bounded_skips(body.skipped), None, "{raw:?}");
        }

        // The whole exchange keyed exactly two ledger buckets: the one
        // configured tab, plus ONE sentinel shared by every identity-less report
        // — the anonymous one and the invented-id one landed in the same bucket.
        assert_eq!(
            ledger.len(),
            2,
            "the key space is the user's tab list plus a sentinel: {:?}",
            ledger.keys().collect::<Vec<_>>()
        );
    }

    /// **Decision 37's bar, clause (6): the response is not the signal.**
    ///
    /// Two halves, and each is load-bearing on its own. The reply is a single
    /// constant reached by the handler's only exit — so a prober learns nothing
    /// from bad JSON, an unknown tab, an anonymous tab, `skipped: 0`,
    /// `skipped: 9999`, a row written or a row suppressed — **while the rows
    /// those inputs produce differ**, which is what stops the property from
    /// being satisfied trivially by a handler that does nothing at all.
    #[test]
    fn the_discovery_report_answers_identically_on_every_path() {
        // Half 1: the bytes. One constant, one exit, no branch before it.
        assert_eq!(DISCOVERY_ACK, br#"{"ok":true}"#);
        let src = include_str!("loopback.rs");
        let body = handler_body(src, "handle_discovery_skipped");
        assert_eq!(
            body.matches("write_").count(),
            1,
            "a second writer is a second response shape: {body}"
        );
        assert!(body.contains("DISCOVERY_ACK"), "{body}");
        for forbidden in ["write_json", "400", "return ", "?;", "if ", "match "] {
            assert!(
                !body.contains(forbidden),
                "`{forbidden}` in this handler is a path the reply could diverge on: {body}"
            );
        }

        // Half 2: the rows DO differ, so the constant reply is hiding something.
        let s = settings_with_tabs(&["f32-t2"]);
        let mut ledger = test_ledger();
        let wrote = |ledger: &mut HashMap<String, outbound::Doubling>, b: &DiscoverySkippedBody| {
            outbound::test_rows::reset();
            let called = std::cell::Cell::new(false);
            record_discovery_skipped(&s, b, |k| claim_in(ledger, k), facts_probe(&called));
            !outbound::test_rows::drain().is_empty()
        };
        // Reports 1 and 2 for this scope write; report 3 is folded into the
        // next power of two. Same reply, different store.
        assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
        assert!(wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)));
        assert!(
            !wrote(&mut ledger, &skipped_body(Some("f32-t2"), 1)),
            "the third report is suppressed — and answers identically"
        );
        assert!(!wrote(&mut ledger, &skipped_body(Some("f32-t2"), 0)));
        outbound::test_rows::reset();
    }

    /// **Decision 37's bar, clauses (3) and (4): a flood costs `log2(n)` rows in
    /// its own lane and none anywhere else.**
    ///
    /// Three halves. The doubling itself, asserted on the `suppressed` counts
    /// and not merely on how many rows appear — a plain global cap would also
    /// produce "a small number". The key space, which is the assertion
    /// `/activity/contract_drift` has no equivalent of and the one that would
    /// have caught F-37. And the lane, which comes free with the `Screen`
    /// variant and is proved for every screen by
    /// `activity::tests::no_screen_can_evict_another_screens_rows`.
    #[test]
    fn a_flood_of_discovery_reports_costs_log2_rows_and_evicts_nothing() {
        // Half 1 — the ledger. 200 reports on one key write 8 rows, at 1, 2, 4,
        // 8, 16, 32, 64, 128, and each names how many it stands for.
        let mut ledger = test_ledger();
        let mut written: Vec<(u32, u32)> = Vec::new();
        for _ in 0..200 {
            if let outbound::DoublingRow::Write { total, suppressed } =
                claim_in(&mut ledger, "claude:one-scope")
            {
                written.push((total, suppressed));
            }
        }
        assert_eq!(
            written,
            vec![
                (1, 0),
                (2, 0),
                (4, 1),
                (8, 3),
                (16, 7),
                (32, 15),
                (64, 31),
                (128, 63)
            ],
            "the magnitude of a loop must survive in the window, not be inferred \
             from the absence of rows"
        );

        // Half 2 — the key space. Ten thousand DISTINCT invented tab ids get ONE
        // bucket and log2-many rows, because the key is the identity the app
        // resolved and not the string the caller typed.
        outbound::test_rows::reset();
        let s = settings_with_tabs(&["f32-t3"]);
        let mut invented = test_ledger();
        for i in 0..10_000u32 {
            let called = std::cell::Cell::new(false);
            record_discovery_skipped(
                &s,
                &skipped_body(Some(&format!("invented-{i}")), 1),
                |k| claim_in(&mut invented, k),
                facts_probe(&called),
            );
        }
        assert_eq!(
            invented.len(),
            1,
            "ten thousand invented ids must not buy ten thousand counters: {:?}",
            invented.keys().collect::<Vec<_>>()
        );
        let rows = outbound::test_rows::drain();
        assert_eq!(
            rows.len(),
            14,
            "10 000 reports must cost log2 rows, not 10 000"
        );
        assert!(rows.iter().all(|r| r.entry.source == "discovery_skipped"));

        // Half 3 — the lane. Declaring the variant is what buys it; the
        // every-screen-against-every-screen matrix in `activity` covers this one
        // as both flooder and victim with no edit to that test, which is the
        // property being relied on here.
        assert!(
            outbound::Screen::ALL.contains(&outbound::Screen::DiscoverySkipped),
            "a screen missing from ALL shares the catch-all lane instead of \
             getting its own guaranteed window"
        );
    }

    // ── #48 F-37 / locked decision 42 — the contract-drift ledger ────────────

    fn drift_ledger() -> HashMap<&'static str, outbound::Doubling> {
        HashMap::new()
    }

    fn drift_body(shim: &str, missing: &[&str], session: Option<&str>) -> ContractDriftBody {
        ContractDriftBody {
            shim: shim.to_string(),
            missing: missing.iter().map(|m| (*m).to_string()).collect(),
            session_id: session.map(str::to_string),
        }
    }

    /// **F-37's whole point: the KEY SPACE, not the map's size.**
    ///
    /// The old ledger was a `HashSet<(shim, session_id)>` with both halves off
    /// the wire, so a token-holder could mint unlimited entries and evict the
    /// 400-row graph lane — genuine security rows included. The bar is the one
    /// `/activity/discovery_skipped` already meets: key on something the caller
    /// does not control.
    ///
    /// Three halves. The doubling, asserted on the `suppressed` counts rather
    /// than on how many rows appear (a plain global cap would also produce "a
    /// small number"). The key space, asserted as **membership of a compile-time
    /// list** and not as `len() < something` — an implementation that merely
    /// evicted or cleared a caller-keyed map when it got big would still hold
    /// caller strings, and would pass a size assertion. And the total ceiling,
    /// which is what "bounded" means here: five shims plus one sentinel, for
    /// every possible input, forever.
    ///
    /// **What this would still pass if the implementation were wrong:** it does
    /// not check the row's *contents* (that is the next test), and it does not
    /// check that the sentinel is shared by the *right* strings — a classifier
    /// that sent every name including the real ones to the sentinel would pass
    /// halves 1 and 2 and fail half 3.
    #[test]
    fn a_flood_of_contract_drift_reports_keys_a_fixed_list_and_costs_log2_rows() {
        // Half 1 — the doubling. 200 reports on one shim write 8 rows, at 1, 2,
        // 4 … 128, each naming how many it stands for.
        let mut ledger = drift_ledger();
        let mut written: Vec<(u32, u32)> = Vec::new();
        for _ in 0..200 {
            if let outbound::DoublingRow::Write { total, suppressed } =
                drift_claim_in(&mut ledger, "read_hook")
            {
                written.push((total, suppressed));
            }
        }
        assert_eq!(
            written,
            vec![
                (1, 0),
                (2, 0),
                (4, 1),
                (8, 3),
                (16, 7),
                (32, 15),
                (64, 31),
                (128, 63)
            ],
            "the magnitude of a flood must survive in the window, not be inferred \
             from the absence of rows"
        );

        // Half 2 — the key space. Ten thousand invented shims, each with its own
        // invented session (the old key's second half), get ONE bucket and
        // log2-many rows, because the key is a classification and not a string
        // the caller typed.
        let mut invented = drift_ledger();
        let mut rows = 0;
        for i in 0..10_000u32 {
            let body = drift_body(
                &format!("invented-{i}"),
                &["session_id"],
                Some(&format!("sess-{i}")),
            );
            if contract_drift_row(&body, |k| drift_claim_in(&mut invented, k)).is_some() {
                rows += 1;
            }
        }
        assert_eq!(
            invented.len(),
            1,
            "ten thousand invented names must not buy ten thousand counters: {:?}",
            invented.keys().collect::<Vec<_>>()
        );
        assert_eq!(invented.keys().copied().collect::<Vec<_>>(), [DRIFT_SHIM_UNKNOWN]);
        assert_eq!(rows, 14, "10 000 reports must cost log2 rows, not 10 000");

        // Half 3 — the ceiling. Every real shim keeps its own counter; everything
        // else shares one; and no input of any kind can key anything else,
        // because the key type is `&'static str` from `DRIFT_SHIMS`.
        let mut all = drift_ledger();
        for shim in DRIFT_SHIMS {
            let body = drift_body(shim, &["cwd"], None);
            assert!(contract_drift_row(&body, |k| drift_claim_in(&mut all, k)).is_some());
        }
        for junk in ["", "   ", "read_hook ", "READ_HOOK", "read", "read_hook2", "🙂"] {
            let body = drift_body(junk, &["cwd"], Some("s"));
            let _ = contract_drift_row(&body, |k| drift_claim_in(&mut all, k));
        }
        assert_eq!(
            all.len(),
            DRIFT_SHIMS.len() + 1,
            "the key space is the shim list plus one sentinel: {:?}",
            all.keys().collect::<Vec<_>>()
        );
        for key in all.keys() {
            assert!(
                DRIFT_SHIMS.contains(key) || *key == DRIFT_SHIM_UNKNOWN,
                "a caller-supplied string reached the ledger's key space: {key}"
            );
        }
        // Trimming is the one normalisation, and it is not a prefix rule:
        // an invented name that merely starts with a real one is the sentinel.
        assert_eq!(drift_shim_key("  read_hook  "), "read_hook");
        assert_eq!(drift_shim_key("read_hook-forged"), DRIFT_SHIM_UNKNOWN);
        assert_eq!(
            drift_shim_key(&format!("read_hook{}", "x".repeat(5_000))),
            DRIFT_SHIM_UNKNOWN,
            "classification must see the whole string, never a truncation of it"
        );
    }

    /// **The string half of the same class: what one drift report may put IN the
    /// row.**
    ///
    /// `ActivityStore::record` truncates `request` and `response` and **not**
    /// `target` — and `target` is what `ipc::commands::advisor_signals` copies
    /// verbatim into a user-facing signal. So the shim name, the session id and
    /// the whole `missing` list reached a capped ring at whatever length a caller
    /// chose. Bounded here, after classification, exactly like F-39's id.
    ///
    /// **What this would still pass if the implementation were wrong:** it says
    /// nothing about *control characters* in those strings — that is Phase D's
    /// concern at the surfaces that render, and `bounded_id`'s doc says so. It
    /// would also pass a bound applied before classification, which is why the
    /// key-space test above asserts the ordering separately.
    #[test]
    fn a_forged_contract_drift_report_cannot_choose_how_many_bytes_a_row_costs() {
        // The honest case first: byte-identical to the plain `join(", ")` this
        // replaced, so the bound costs a real report nothing.
        let mut ledger = drift_ledger();
        let real = contract_drift_row(
            &drift_body("read_hook", &["session_id", "cwd"], Some("sess-1")),
            |k| drift_claim_in(&mut ledger, k),
        )
        .expect("the first report from a shim always writes");
        assert_eq!(real.entry.target, "read_hook: session_id, cwd");
        assert_eq!(real.entry.source, "harness");
        assert_eq!(real.entry.tool, "contract_drift");
        assert_eq!(real.entry.session.as_deref(), Some("sess-1"));
        assert_eq!(real.entry.tab, crate::activity::Attribution::Unattributed);
        assert!(!real.entry.ok, "a drift report is never `ok`");
        assert!(
            real.request.contains("report 1 from this shim this app run, 0 folded into it"),
            "a folded report must be countable from the row that stands for it: {}",
            real.request
        );

        // The forged case: every caller-supplied string bounded, and the row
        // still filed under the sentinel rather than under `read_hook`.
        let huge_missing: Vec<String> = (0..5_000).map(|i| format!("{}{i}", "f".repeat(4096))).collect();
        let borrowed: Vec<&str> = huge_missing.iter().map(String::as_str).collect();
        let long = "x".repeat(4096);
        let mut forged_ledger = drift_ledger();
        let forged = contract_drift_row(
            &drift_body(&long, &borrowed, Some(&long)),
            |k| drift_claim_in(&mut forged_ledger, k),
        )
        .expect("a first report writes");
        assert_eq!(
            forged_ledger.keys().copied().collect::<Vec<_>>(),
            [DRIFT_SHIM_UNKNOWN]
        );
        // `shim: ` + at most MAX_DRIFT_MISSING bounded names + the overflow note.
        let ceiling = (BEACON_TOOL_MAX + 1) * (MAX_DRIFT_MISSING + 1) + 64;
        assert!(
            forged.entry.target.chars().count() <= ceiling,
            "{} chars reached a row the store does not truncate",
            forged.entry.target.chars().count()
        );
        assert!(
            forged.entry.session.as_deref().unwrap().chars().count() <= BEACON_TOOL_MAX + 1,
            "the session column is a join key, not a payload"
        );
        assert!(
            forged.entry.target.contains("(+4988 more)"),
            "a cut list must say how much was cut: {}",
            forged.entry.target
        );

        // A shim that sends nothing but empty strings still produces a row that
        // reads honestly — "empty" must not be spelled the same way as a name.
        let mut empty_ledger = drift_ledger();
        let empty = contract_drift_row(&drift_body("", &[], None), |k| {
            drift_claim_in(&mut empty_ledger, k)
        })
        .expect("a first report writes");
        assert_eq!(empty.entry.target, ": ");
        assert_eq!(empty.entry.session.as_deref(), Some(""));
    }

    /// The shim names in [`DRIFT_SHIMS`] are the ones the shims actually send.
    ///
    /// A typo would not break anything loudly — the misspelt entry would simply
    /// never be claimed and its shim would share the sentinel — which is exactly
    /// why it needs a tripwire rather than a reader's attention.
    ///
    /// **What this would still pass if the implementation were wrong:** a NEW
    /// shim that reports drift under an unlisted name. That case degrades safely
    /// (it shares the sentinel bucket, so it gets fewer rows, never more) and
    /// cannot be enumerated from here without scanning the source tree at test
    /// time. **It happened**: `checkpoint_beacon` shipped with V33 Phase F
    /// reporting drift under its own name and no entry in the list, which is why
    /// the length assertion below now names the failure it is guarding rather
    /// than just pinning a number.
    #[test]
    fn the_drift_shim_list_is_spelled_the_way_the_shims_spell_it() {
        for (shim, src) in [
            ("checkpoint_beacon", include_str!("../checkpoint_beacon.rs")),
            ("compact_hook", include_str!("../compact_hook.rs")),
            ("context_hook", include_str!("../context_hook.rs")),
            ("notify_hook", include_str!("../notify_hook.rs")),
            ("read_hook", include_str!("../read_hook.rs")),
            ("taint_beacon", include_str!("../taint_beacon.rs")),
        ] {
            assert!(
                DRIFT_SHIMS.contains(&shim),
                "{shim} reports drift and has no counter"
            );
            assert!(
                src.contains(&format!("\"{shim}\"")),
                "{shim} is not the name that module sends"
            );
        }
        assert_eq!(
            DRIFT_SHIMS.len(),
            6,
            "a new shim that reports drift needs BOTH an entry here and its source \
             pinned in the loop above — otherwise its reports fold into the \
             unrecognized-shim bucket and nothing fails"
        );
        assert!(!DRIFT_SHIMS.contains(&DRIFT_SHIM_UNKNOWN));
    }

    // ── V35 Phase I — CHP: the hello row and the observation seam ────────────

    /// A hello writes ONE row, says what the artifact declared, and a
    /// flip-flopping caller costs `log2(n)` rows rather than one per hello.
    ///
    /// The row shape matters as much as the bound: the target has to name the
    /// tab, the protocol version and the sizes of the two declaration lists,
    /// because that is what a reader diffing "before the upgrade / after the
    /// upgrade" actually compares.
    #[test]
    fn a_hello_row_names_the_version_and_costs_log2_rows_under_a_flood() {
        let mut ledger: HashMap<String, outbound::Doubling> = HashMap::new();
        let serves = vec!["prompt".to_string(), "tool.gate".to_string()];
        let first = hello_row(
            "opencode",
            "opencode-1",
            crate::harness::chp::CHP_VERSION,
            "1.18.13",
            &serves,
            1,
            |k| claim_in(&mut ledger, k),
        )
        .expect("the first hello writes a row");
        assert_eq!(first.entry.source, "harness");
        assert_eq!(first.entry.tool, "chp_hello");
        assert_eq!(
            first.entry.kind,
            crate::activity::ActivityKind::Graph.as_str(),
            "the hello rides the lane `contract_drift` already uses for harness facts"
        );
        assert!(first.entry.ok, "a hello is a healthy event, not a flag");
        assert_eq!(
            first.entry.tab,
            crate::activity::Attribution::Tab("opencode-1".to_string()),
            "the tab was validated against the configured list before this point"
        );
        assert!(first.entry.target.contains("chp 1"), "{}", first.entry.target);
        assert!(first.entry.target.contains("v1.18.13"), "{}", first.entry.target);
        assert!(first.entry.target.contains("serves 2"), "{}", first.entry.target);
        assert!(first.entry.target.contains("cannot 1"), "{}", first.entry.target);
        assert!(first.request.contains("tool.gate"));
        // An undeclared version says so rather than rendering as an empty `v`.
        let quiet = hello_row("claude", "claude-1", 0, "", &[], 0, |k| {
            claim_in(&mut ledger, k)
        })
        .expect("a second key gets its own counter");
        assert!(
            quiet.entry.target.contains("version not declared"),
            "{}",
            quiet.entry.target
        );
        assert!(quiet.request.contains("(nothing declared)"));

        // The bound: 200 hellos from ONE tab write 8 rows, each stating how many
        // it stands for. The key is `agent:tab` and the tab is only ever reached
        // after `is_configured_tab` accepted it, so the key space is the user's
        // own tab list.
        let mut flood: HashMap<String, outbound::Doubling> = HashMap::new();
        let rows = (0..200)
            .filter(|_| {
                hello_row("opencode", "opencode-1", 1, "", &[], 0, |k| {
                    claim_in(&mut flood, k)
                })
                .is_some()
            })
            .count();
        assert_eq!(rows, 8, "a re-hellowing plugin must cost log2 rows, not 200");
        assert_eq!(flood.len(), 1, "one tab, one counter");
    }

    /// The declaration lists are bounded before they reach the peer registry or
    /// the Settings panel — the [`bounded_missing`] discipline, one route over.
    #[test]
    fn a_hellos_declarations_cannot_choose_how_much_of_the_panel_they_occupy() {
        let huge: Vec<String> = (0..500).map(|i| format!("{}-{i}", "x".repeat(400))).collect();
        let bounded = bounded_declarations(&huge);
        assert_eq!(bounded.len(), MAX_HELLO_DECLARATIONS);
        assert!(
            bounded.iter().all(|s| s.chars().count() <= BEACON_TOOL_MAX + 1),
            "each entry is truncated like every other caller-supplied id"
        );
        assert!(bounded_declarations(&[]).is_empty());
    }

    /// The observation seam, in the two directions that matter for "zero
    /// behavior change": a pre-CHP body is READ, not rejected, and a route that
    /// is not CHP is not observed at all.
    ///
    /// The handler-side half (tab validation, the settings read) needs an
    /// `AppHandle` and is covered by the route-surface tests plus
    /// `harness::chp`'s own suite; what is asserted here is that the loopback's
    /// pre-dispatch hook agrees with the protocol module about *what counts as a
    /// CHP message*, which is the seam a new route would silently fall out of.
    #[test]
    fn the_chp_observation_reads_every_body_the_routes_actually_send() {
        // Exactly the body `context_hook.rs` builds today — no `chp`.
        let (env, tab) = crate::harness::chp::envelope(
            "/context/retrieve",
            br#"{"cwd":"P:\\p","prompt":"hi","session_id":"s","agent":"claude","tab":"claude-1"}"#,
        )
        .expect("the pre-CHP Claude body is still observable");
        assert_eq!(env.chp, None);
        assert_eq!(crate::graph::source_for_consumer(env.agent_token()), "claude");
        assert_eq!(tab, "claude-1");

        // …and the body the generated plugin now sends.
        let (env, _) = crate::harness::chp::envelope(
            "/latch/beacon",
            br#"{"chp":1,"tab":"opencode-1","consumer":"opencode","tool":"webfetch"}"#,
        )
        .expect("the plugin's beacon body");
        assert_eq!(env.chp, Some(crate::harness::chp::CHP_VERSION));
        assert_eq!(
            crate::graph::source_for_consumer(env.agent_token()),
            "opencode"
        );

        // Every route the containment table declares as CHP-carrying is one the
        // protocol module agrees to observe, and none of the non-CHP ones is.
        for row in ROUTE_CONTAINMENT {
            let observed = crate::harness::chp::is_push_route(row.path);
            let expected = matches!(
                row.path,
                "/context/retrieve"
                    | "/context/compaction"
                    | "/context/should_read"
                    | "/context/post_edit"
                    | "/memory/event"
                    | "/permission/event"
                    | "/latch/beacon"
                    | "/latch/state"
                    | "/workbench/tool_checkpoint"
                    | "/activity/contract_drift"
            );
            assert_eq!(
                observed, expected,
                "`{}` disagrees with `harness::chp::EVENTS` about whether it carries CHP — a new \
                 route either belongs in the vocabulary (with a row in docs/CHP.md) or in this \
                 list's negative half, deliberately",
                row.path
            );
        }
    }

    /// **The positive case, on the wire, and its negative control.** A skipped
    /// planted entry reaches the app; a clean resolution says nothing.
    ///
    /// Asserted on the BYTES a listening instance receives. `SKIPPED_CANDIDATES`
    /// is deliberately not asserted anywhere here: that counter is F-11's, it
    /// already works, and *the entire finding is that it has no user consumer* —
    /// a test that asserts the counter re-pins the defect's shape.
    #[test]
    fn a_skipped_entry_is_reported_on_the_wire_and_a_clean_resolution_says_nothing() {
        let (port, seen) = recording_instance("tok-f32");
        let d = Discovery {
            port,
            token: "tok-f32".into(),
            pid: 4242,
            root: "P:\\proj".into(),
        };
        let who = ChildIdentity {
            consumer: "claude",
            tab: Some("claude-1"),
        };

        dispatch_discovery_report(&d, who, 2);
        let req = wait_for_request(&seen, 1).expect("the live instance receives the report");
        assert!(
            req.starts_with("POST /activity/discovery_skipped HTTP/1.1\r\n"),
            "{req}"
        );
        assert!(req.contains("Authorization: Bearer tok-f32\r\n"), "{req}");
        assert!(req.contains("\"skipped\":2"), "{req}");
        assert!(req.contains("\"tab\":\"claude-1\""), "{req}");
        assert!(req.contains("\"consumer\":\"claude\""), "{req}");
        // Nothing else rides along: no cwd, no path, no free text, and no
        // pid/port/root of the entries that were skipped.
        for absent in ["cwd", "root", "pid", "port", "session"] {
            assert!(!req.contains(absent), "`{absent}` on the wire: {req}");
        }

        // The count is clamped by the CHILD too, so the wire is honest rather
        // than relying on the far end to fix it.
        dispatch_discovery_report(&d, who, 9999);
        let req = wait_for_request(&seen, 2).expect("a second report");
        assert!(req.contains("\"skipped\":6"), "{req}");

        // A child with no `--tab` sends no `tab` key at all — absent, not null.
        dispatch_discovery_report(
            &d,
            ChildIdentity {
                consumer: "opencode",
                tab: None,
            },
            1,
        );
        let req = wait_for_request(&seen, 3).expect("a third report");
        assert!(!req.contains("\"tab\""), "{req}");
        assert!(req.contains("\"consumer\":\"opencode\""), "{req}");

        // **The negative control.** A resolution that skipped nothing posts
        // nothing — without this half, an implementation that reported
        // unconditionally would pass everything above.
        report_skipped_to_app(&d, who, 0);
        assert!(
            wait_for_request(&seen, 4).is_none(),
            "a clean resolution must be silent: {:?}",
            seen.lock().unwrap_or_else(PoisonError::into_inner).len()
        );

        // **Fail-open.** The endpoint is dead at report time: the dispatcher
        // returns normally, quickly, and reports nothing back to its caller —
        // its return type is `()`, so it cannot fail the child's real work.
        let dead = Discovery {
            port: dead_port(),
            ..d
        };
        let t0 = std::time::Instant::now();
        dispatch_discovery_report(&dead, who, 1);
        assert!(
            t0.elapsed() < DISCOVERY_REPORT_TIMEOUT * 4,
            "a dead endpoint must cost the stated bound at most: {:?}",
            t0.elapsed()
        );
    }

    /// **The hard constraint, as behaviour rather than a source scan for a call
    /// that is not there.** The report must never appear on the hook shims'
    /// resolution path: `taint_beacon`'s entire safety argument is that it
    /// writes nothing to stdout or stderr and awaits nothing, and it reaches
    /// discovery through `read_discovery_for`, not through `proxy_base_for`.
    ///
    /// A `grep` for `dispatch(` in `taint_beacon.rs` would stay green while a
    /// refactor moved the POST one frame down into the shared resolver. The
    /// socket assertion would not.
    #[test]
    fn the_discovery_report_never_reaches_the_hook_shims_path() {
        // The shims' own resolution shape, against a real socket: one probe, and
        // nothing else, ever leaves this path.
        let (port, seen) = recording_instance("tok-shim");
        let live = Discovery {
            port,
            token: "tok-shim".into(),
            pid: 10,
            root: proj("proj"),
        };
        let planted = disc(4242, dead_port(), &proj("proj/sub"));
        let cwd = proj_path("proj/sub/pkg");
        let picked = select_verified(vec![live, planted], Some(&cwd), responds, || None)
            .expect("the shim still finds the running instance");
        assert_eq!(picked.port, port);
        std::thread::sleep(Duration::from_millis(250));
        let reqs = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(reqs.len(), 1, "the resolver sent more than a probe: {reqs:?}");
        assert!(reqs[0].starts_with("GET /health "), "{}", reqs[0]);

        // …and structurally: the ONE report site sits in `proxy_base_for`, after
        // the `?` that proves an endpoint resolved.
        let src = include_str!("loopback.rs");
        let resolver = top_level_fn(src, "pub fn proxy_base_for(");
        let after_q = resolver.find("let d = d?;").expect("the `?` is the guard");
        let report = resolver
            .find("report_skipped_to_app(")
            .expect("the report site");
        assert!(
            report > after_q,
            "reporting before the `?` would fire with no endpoint to report to"
        );
        assert!(
            !top_level_fn(src, "pub fn read_discovery_for(").contains("report_skipped_to_app"),
            "the shims call `read_discovery_for`; a report there is a write and a \
             wait inside `taint_beacon`"
        );
        // The production ledger really is what the route claims: the process-wide
        // doubling map, not a per-call one that would bound nothing.
        assert!(
            top_level_fn(src, "fn note_discovery_skipped(").contains("claim_discovery_report"),
            "the handler must claim against the process ledger"
        );
    }

    /// A [`fake_instance`] that also **records every request it received**, so a
    /// test can assert on what left a code path rather than on what that path
    /// decided. Answers exactly as `fake_instance` does.
    #[allow(clippy::type_complexity)]
    fn recording_instance(token: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 2048];
                let n = conn.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(req);
                let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    if ok { 200 } else { 401 },
                    if ok { "OK" } else { "Unauthorized" },
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(body);
            }
        });
        (port, seen)
    }

    /// Wait up to 250 ms for the `n`th request to arrive, since the dispatcher
    /// never waits for the peer. `None` ⇒ it never came, which is the assertion
    /// the negative control needs.
    fn wait_for_request(seen: &Arc<Mutex<Vec<String>>>, n: usize) -> Option<String> {
        for _ in 0..50 {
            if let Some(req) = seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(n - 1)
            {
                return Some(req.clone());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    /// The source text of one top-level `fn`, signature to closing brace — the
    /// non-`async` twin of [`handler_body`], with the same CRLF normalisation
    /// and the same "a `}` in column 0 ends it" rule.
    fn top_level_fn(src: &str, sig: &str) -> String {
        let mut out = String::new();
        let mut inside = false;
        for line in src.lines() {
            if !inside {
                if !line.starts_with(sig) {
                    continue;
                }
                inside = true;
            }
            out.push_str(line);
            out.push('\n');
            if line == "}" {
                break;
            }
        }
        assert!(!out.is_empty(), "no top-level `{sig}`");
        assert!(out.ends_with("}\n"), "`{sig}` was not terminated");
        out
    }

    /// A port on the loopback interface that nothing is listening on: bound,
    /// read, and released before the test uses the number. A `connect` to it is
    /// refused immediately, which is also why a dead planted entry costs the
    /// probe budget almost nothing in practice.
    fn dead_port() -> u16 {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = l.local_addr().expect("addr").port();
        drop(l);
        port
    }

    /// A minimal stand-in for a running cImp instance: answers `GET /health`
    /// with 200 when the bearer token matches and 401 when it does not, exactly
    /// like `handle_conn` + `write_simple` do. Returns the port it bound.
    ///
    /// A real socket rather than a stubbed closure because the property under
    /// test is what [`responds`] puts on the wire — a stub would pass even if the
    /// probe sent no `Authorization` header at all.
    ///
    /// The accept loop runs on a **detached** thread and lives until the test
    /// binary exits. Deliberate: a joinable guard would have to interrupt a thread
    /// parked in `accept`, and a test that can hang waiting on its own diagnostic
    /// helper is worse than one leaked thread per test.
    fn fake_instance(token: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut buf = [0u8; 512];
                let n = conn.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let ok = req.contains(&format!("Authorization: Bearer {token}\r\n"));
                let body: &[u8] = if ok { b"ok" } else { b"unauthorized" };
                let head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    if ok { 200 } else { 401 },
                    if ok { "OK" } else { "Unauthorized" },
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(body);
            }
        });
        port
    }

    #[test]
    fn is_ancestor_or_equal_rejects_prefix_strings_and_unrelated() {
        assert!(is_ancestor_or_equal(
            &proj_path("proj/a"),
            &proj_path("proj/a")
        ));
        // Ancestry across a real component boundary — the case a hard-coded
        // `P:\proj\a` literal cannot express off Windows, where the whole
        // string is one component and this would pass vacuously.
        assert!(is_ancestor_or_equal(
            &proj_path("proj"),
            &proj_path("proj/a/deep")
        ));
        // Component-wise, not string-prefix: `<root>/a` is NOT an ancestor
        // of `<root>/ab`.
        assert!(!is_ancestor_or_equal(
            &proj_path("proj/a"),
            &proj_path("proj/ab")
        ));
        assert!(!is_ancestor_or_equal(
            &proj_path("proj/a/deep"),
            &proj_path("proj/a")
        ));
        assert!(!is_ancestor_or_equal(Path::new(""), &proj_path("proj")));
    }

    /// **V33: an unresolved `..` is refused by the ancestry walk itself, so
    /// both routes that use it inherit the refusal.**
    ///
    /// [`canon`] resolves `..` only for a path that EXISTS — `canonicalize`
    /// fails on anything else and the raw string is kept — so a `..` reaches
    /// this walk intact and a plain zip-compare calls it a descendant. Both
    /// [`audit_admit`] step 3 and [`admitted_hook_root`] feed caller-supplied
    /// strings in, which is why the answer lives in one place.
    ///
    /// The Windows case is the one worth pinning, because it is the one that
    /// looks safe and is not. `canon` adds a `\\?\` verbatim prefix on success
    /// and not on failure, so the *plain* `P:\proj\..\..\evil` is rejected only
    /// as a side effect of the prefixes disagreeing — nothing to do with `..`.
    /// Spell the prefix yourself and the accident evaporates: before this
    /// refusal, `\\?\P:\proj\..\..\evil` matched `\\?\P:\proj` and walked
    /// through. Off Windows there is no prefix at all and the plain spelling
    /// walked through too, so this is not a Windows-only property and its test
    /// is not Windows-only either.
    #[test]
    fn is_ancestor_or_equal_refuses_an_unresolved_parent_dir() {
        let root = proj_path("proj");

        // The plain spelling, on either platform.
        assert!(!is_ancestor_or_equal(
            &root,
            &root.join("..").join("..").join("evil")
        ));
        // A `..` that does not even leave the root is still refused: this walk
        // cannot tell the difference, and every real caller sends a resolved
        // absolute path.
        assert!(!is_ancestor_or_equal(
            &root,
            &root.join("sub").join("..").join("evil")
        ));
        // The `root` side too — a discovery entry's `root` is file-supplied
        // (decision 30) and reaches `select_answering` unfiltered.
        assert!(!is_ancestor_or_equal(
            &root.join(".."),
            &proj_path("proj/a/deep")
        ));

        // Windows: the same escape with the verbatim prefix supplied by the
        // caller, which is what `canon` produces for the root side. This is the
        // spelling that actually matched before the refusal landed.
        if cfg!(windows) {
            assert!(!is_ancestor_or_equal(
                Path::new(r"\\?\P:\proj"),
                Path::new(r"\\?\P:\proj\..\..\evil")
            ));
            // Control: the same pair without the `..` still matches, so the
            // assertion above is about `..` and not about the prefix.
            assert!(is_ancestor_or_equal(
                Path::new(r"\\?\P:\proj"),
                Path::new(r"\\?\P:\proj\src")
            ));
        }
    }

    /// **V33: `/audit/run`'s step 3 gets the `..` refusal from the shared
    /// helper, not from a copy of `/context/post_edit`'s check.**
    ///
    /// The two routes answer a miss differently (a readable tool error here, an
    /// empty-text fail-safe there) but they must agree on what a miss IS. This
    /// asserts the agreement at the route, so deleting the shared refusal fails
    /// here as well as at [`admitted_hook_root`]'s own test.
    ///
    /// **What this deliberately does NOT claim.** Step 3 is a wrong-instance
    /// guard, not a boundary: `cwd` is optional on the wire, so a body that
    /// omits it skips the check entirely — and gains nothing by it, since the
    /// scan root is `served_root` either way. The property pinned here is
    /// consistency between the two path checks, not containment.
    #[test]
    fn audit_run_refuses_a_traversal_cwd_like_post_edit_does() {
        // A REAL directory on both sides, deliberately: `canon` then SUCCEEDS
        // for the served root and for the control cwd, so on Windows both carry
        // the `\\?\` prefix and the escape below is decided by the component
        // walk rather than by the prefix accident this fix exists to stop
        // relying on. A synthetic `P:\proj` would test the accident instead.
        let served = std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")))
            .expect("the crate's own directory exists");
        let body = |cwd: &str| AuditRunBody {
            category: crate::audit::adapters::Category::Security,
            consumer: Some("claude".into()),
            cwd: Some(cwd.to_string()),
            tab: Some("tab-1".into()),
        };
        let admit = |b: &AuditRunBody| {
            audit_admit(
                &LatchRegistry::default(),
                b,
                &served,
                |_| true,
                |_, _| LatchScoping::Anonymous,
                |_| ON,
            )
        };

        // Control: a cwd inside the served root is admitted.
        let inside = served.join("src").to_string_lossy().into_owned();
        assert!(admit(&body(&inside)).is_ok(), "{inside}");

        // The traversal — spelled from the canonicalized root, i.e. WITH the
        // verbatim prefix on Windows — takes the wrong-instance refusal.
        let sep = if cfg!(windows) { '\\' } else { '/' };
        let escape = format!("{}{sep}..{sep}..{sep}evil", served.display());
        let err = admit(&body(&escape)).expect_err(&escape);
        assert!(
            err.contains("this cImp instance serves"),
            "{escape} must take the wrong-instance refusal, got: {err}"
        );
    }

    #[test]
    fn audit_run_body_parses_both_categories_and_rejects_junk() {
        use crate::audit::adapters::Category;
        // Both wire categories deserialize. `consumer` and `tab` stay optional
        // *on the wire* — H-8 enforces them in `audit_admit` instead, so a body
        // missing either becomes the route's readable tool error rather than a
        // bare 400 the model cannot act on. Only `category` is a parse error.
        let sec: AuditRunBody =
            serde_json::from_slice(br#"{"category":"security","consumer":"claude"}"#).unwrap();
        assert_eq!(sec.category, Category::Security);
        assert_eq!(sec.consumer.as_deref(), Some("claude"));
        let qual: AuditRunBody = serde_json::from_slice(br#"{"category":"quality"}"#).unwrap();
        assert_eq!(qual.category, Category::Quality);
        assert!(
            qual.consumer.is_none(),
            "consumer defaults to None when absent"
        );
        // A bad category word (or a missing `category`) is a clean parse error →
        // the route answers 400.
        assert!(serde_json::from_slice::<AuditRunBody>(br#"{"category":"bogus"}"#).is_err());
        assert!(serde_json::from_slice::<AuditRunBody>(br#"{"consumer":"x"}"#).is_err());
    }

    #[test]
    fn graph_run_body_round_trips_the_v28_tab_field() {
        // V28: the per-tab MCP child tags `/graph_run` with the tab it serves.
        let tagged: GraphRunBody = serde_json::from_slice(
            br#"{"cwd":"P:\\proj","name":"context_recall","args":{},"consumer":"opencode","tab":"opencode"}"#,
        )
        .expect("tagged body parses");
        assert_eq!(tagged.tab.as_deref(), Some("opencode"));
        assert_eq!(tagged.consumer.as_deref(), Some("opencode"));
        assert_eq!(tagged.name, "context_recall");
    }

    #[test]
    fn graph_run_body_still_accepts_pre_v28_bodies() {
        // Fail-open on the wire: a child spawned before the upgrade (or by hand)
        // sends no `tab` at all, and an explicit `null` must read the same. Both
        // resolve to `None`, i.e. the pre-V28 most-recent-session scoping — never
        // a 400 that would break the tool call.
        let absent: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"consumer":"claude"}"#)
                .expect("pre-V28 body still parses");
        assert!(absent.tab.is_none());
        assert!(absent.cwd.is_none());

        let null: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"tab":null}"#)
                .expect("explicit null parses");
        assert!(null.tab.is_none());

        // An unknown extra field (a NEWER child talking to an older app) is
        // likewise tolerated rather than rejected.
        let extra: GraphRunBody =
            serde_json::from_slice(br#"{"name":"context_notes","args":{},"future_field":1}"#)
                .expect("unknown fields ignored");
        assert!(extra.tab.is_none());
    }

    // ── V32 Phase B — the proxy's per-session taint latch ──────────────────

    use crate::offload::toolclass::{
        REFUSAL_EXTERNAL_BLOCKED, REFUSAL_EXTERNAL_USER_LOCAL, REFUSAL_LOCAL_BLOCKED,
    };

    /// A scope for `tab`, claiming session `session` (`None` = the registry
    /// withheld one). `claude` unless the test says otherwise.
    fn scope(tab: &str, session: Option<&str>) -> LatchScope {
        LatchScope {
            agent: "claude",
            tab: tab.to_string(),
            session: session.map(str::to_string),
            root: TEST_ROOT.to_string(),
        }
    }

    // ── V32 Phase G — the two switches over this gate ──────────────────────

    /// The taint latch OFF: nothing latches, nothing is refused, and — because
    /// an inert policy must leave no trace — `/status` does not sprout a row
    /// showing a boundary that is not being enforced.
    #[test]
    fn a_disabled_latch_refuses_nothing_and_records_nothing() {
        let off = GatePolicy {
            latch: false,
            quarantine: false,
        };
        let reg = LatchRegistry::default();
        let s = scope("claude-off", Some("ses"));
        // The classic fetch-then-read sequence, which under ON closes the local
        // side after the first EXTERNAL call.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                off,
                NO_CONTENT
            )
            .is_ok());
        for name in ["graph_snippet", "read_file", "context_note", "ddg__search"] {
            assert!(
                reg.gate(Some(&s), LatchRoute::Native, name, off, NO_CONTENT)
                    .is_ok(),
                "{name} must not be refused with the latch off"
            );
        }
        assert!(
            reg.snapshot().is_empty(),
            "an inert gate must not create a latch row"
        );
    }

    /// Memory quarantine OFF: a write from a conversation that HAS read external
    /// content is stored clean. (The read-side exclusion is deliberately not
    /// gated — already-held notes stay held; see the Phase G amendment.)
    #[test]
    fn a_disabled_quarantine_stores_a_contaminated_write_clean() {
        let no_quarantine = GatePolicy {
            latch: true,
            quarantine: false,
        };
        let reg = LatchRegistry::default();
        let s = scope("claude-q", Some("ses"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                no_quarantine,
                NO_CONTENT
            )
            .is_ok());
        // The latch still engaged (that is a different feature)…
        assert_eq!(reg.snapshot()[0].latch(), "external");
        // …but the write is not held.
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "context_note",
                no_quarantine,
                NO_CONTENT
            ),
            Ok(WriteTaint::Clean)
        );
    }

    /// The asymmetric combination the two switches exist to allow: latch OFF,
    /// quarantine ON. Nothing is refused, but contamination is still tracked, so
    /// a note written after a fetch is still held for review.
    #[test]
    fn quarantine_survives_a_disabled_latch_via_the_contamination_bit() {
        let quarantine_only = GatePolicy {
            latch: false,
            quarantine: true,
        };
        let reg = LatchRegistry::default();
        let s = scope("claude-mix", Some("ses"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                quarantine_only,
                NO_CONTENT
            )
            .is_ok());
        // The latch itself never moved — it is off.
        assert_eq!(reg.snapshot()[0].latch(), "open");
        assert!(reg.snapshot()[0].view.contaminated);
        // Local tools stay open (no latch)…
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                quarantine_only,
                NO_CONTENT
            )
            .is_ok());
        // …and the write is held anyway.
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "context_note",
                quarantine_only,
                NO_CONTENT
            ),
            Ok(WriteTaint::Quarantined)
        );
    }

    /// A beacon under an inert policy engages nothing and creates no row — the
    /// sensor hook may still be installed on a tab whose latch was switched off
    /// after spawn, and it must not resurrect the feature.
    #[test]
    fn a_beacon_under_an_inert_policy_is_a_no_op() {
        let off = GatePolicy {
            latch: false,
            quarantine: false,
        };
        let reg = LatchRegistry::default();
        let s = scope("claude-beacon-off", Some("ses"));
        assert_eq!(
            reg.beacon(Some(&s), "WebFetch", off, BEACON_PROV),
            BeaconOutcome::inert()
        );
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn mcp_call_body_carries_the_v32_tab_field_and_tolerates_its_absence() {
        // V32 Phase B: the per-tab child now tags `/mcp/call` too, so the
        // proxy can key the call to that tab's session latch.
        let tagged: McpCallBody = serde_json::from_slice(
            br#"{"name":"ddg__fetch_content","arguments":{"url":"x"},"cwd":"P:\\proj","tab":"claude-2"}"#,
        )
        .expect("tagged body parses");
        assert_eq!(tagged.tab.as_deref(), Some("claude-2"));
        assert_eq!(tagged.name, "ddg__fetch_content");

        // Fail-open on the wire, exactly like `/graph_run`: a child from before
        // this field (or an explicit null) must still be served, unlatched.
        let absent: McpCallBody =
            serde_json::from_slice(br#"{"name":"ddg__search","arguments":{}}"#)
                .expect("pre-V32 body still parses");
        assert!(absent.tab.is_none());
        assert!(absent.cwd.is_none());
        let null: McpCallBody =
            serde_json::from_slice(br#"{"name":"ddg__search","arguments":{},"tab":null}"#)
                .expect("explicit null parses");
        assert!(null.tab.is_none());
    }

    /// Direction 1: the tab fetches the web first, so the content-bearing
    /// (LOCAL-CAPABILITY) graph tools close for the rest of that session —
    /// read-after-fetch is how an injected page steers later reads.
    #[test]
    fn external_first_closes_the_local_capability_side_for_that_tab() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());

        for blocked in [
            "graph_snippet",
            "graph_search_docs",
            "graph_semantic_docs",
            "graph_semantic_code",
        ] {
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
                Err(REFUSAL_LOCAL_BLOCKED),
                "{blocked}"
            );
        }
        // The external side itself stays usable — the latch is exclusion, not
        // a kill switch.
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "external");
    }

    /// Direction 2: the tab reads source text first, so the proxied servers
    /// close — read-then-fetch is how secrets ride out on a fetch URL.
    #[test]
    fn local_capability_first_closes_the_external_side_for_that_tab() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());

        for blocked in ["ddg__search", "ddg__fetch_content", "context7__query-docs"] {
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Proxied, blocked, ON, NO_CONTENT),
                Err(REFUSAL_EXTERNAL_BLOCKED),
                "{blocked}"
            );
        }
        // Local work continues, including the memory write (only an EXTERNAL
        // latch gates persistence).
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local");
    }

    /// TRUSTED tools are immune in both directions and never latch anything:
    /// a structural graph query or a memory read must not cost the session
    /// either capability.
    #[test]
    fn trusted_tools_never_latch_and_are_never_refused() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        // V32 H-1: `graph_repo_map` was in this list until the 2026-08-08
        // re-review demoted it out of TRUSTED — see
        // `the_source_text_graph_readers_are_refused_at_the_proxy_gate` below,
        // which asserts the opposite verdict for it and for
        // `graph_struct_search` on this same route.
        for trusted in [
            "graph_outline",
            "graph_find_symbol",
            "context_recall",
            "context_notes",
        ] {
            assert!(
                reg.gate(Some(&s), LatchRoute::Native, trusted, ON, NO_CONTENT)
                    .is_ok(),
                "{trusted}"
            );
        }
        assert!(reg.snapshot().is_empty() || reg.snapshot()[0].latch() == "open");

        // And under a latch of either kind they still answer.
        for (route, first) in [
            (LatchRoute::Proxied, "ddg__search"),
            (LatchRoute::Native, "graph_snippet"),
        ] {
            let reg = LatchRegistry::default();
            let s = scope("t", Some("s"));
            assert!(reg.gate(Some(&s), route, first, ON, NO_CONTENT).is_ok());
            for trusted in ["graph_outline", "context_recall", "context_notes"] {
                assert!(
                    reg.gate(Some(&s), LatchRoute::Native, trusted, ON, NO_CONTENT)
                        .is_ok(),
                    "{trusted} under {first}"
                );
            }
        }
    }

    /// **V32 H-1 (2026-08-08 re-review — C-1 reopened): `graph_struct_search`
    /// and `graph_repo_map` are refused at the TAB gate.**
    ///
    /// This is the second of the two enforcement paths and, for a
    /// Claude/OpenCode tab, the only one: graph tools arrive on `/graph_run`,
    /// which gates by name through [`LatchRegistry::gate`], and the proxy never
    /// def-filters the graph surface (the per-session child caches `tools/list`
    /// at connect). A fix verified only against the worker's `filter_defs` —
    /// which is how C-1 survived `b80f5b8` — would leave this route wide open,
    /// so it is asserted here rather than inferred from the class table.
    #[test]
    fn the_source_text_graph_readers_are_refused_at_the_proxy_gate() {
        for blocked in ["graph_struct_search", "graph_repo_map"] {
            // Contaminated conversation ⇒ refused with the fixed local string.
            let reg = LatchRegistry::default();
            let s = scope("claude-1", Some("sess-a"));
            assert!(reg
                .gate(
                    Some(&s),
                    LatchRoute::Proxied,
                    "ddg__fetch_content",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
                Err(REFUSAL_LOCAL_BLOCKED),
                "{blocked} must be refused once the conversation has read a page"
            );

            // …and used first it LATCHES the tab local, closing the web — the
            // accepted consequence of the demotion for a tab, not just for a
            // worker task.
            let reg = LatchRegistry::default();
            let s = scope("claude-2", Some("sess-b"));
            assert!(reg
                .gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT)
                .is_ok());
            assert_eq!(reg.snapshot()[0].latch(), "local", "{blocked}");
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
                Err(REFUSAL_EXTERNAL_BLOCKED),
                "{blocked}"
            );
        }
    }

    /// **C-1b + C-1c (2026-08-07 re-verification sweep): the two routes that
    /// reached LOCAL-CAPABILITY without ever consulting `classify()`.**
    ///
    /// `b80f5b8` demoted `run_check`/`security_audit`/`quality_audit`, but the
    /// demotion only reached the offload worker's def-filtering path. The audit
    /// tools arrive on `/audit/run` (their own MCP server, `cimp-code-audit`),
    /// which held no `latches()` call at all; `offload_task`/`offload_batch`
    /// arrive on `/run`, which held none either and was TRUSTED besides. Both
    /// routes now gate here, so this pins the verdict both of them read.
    #[test]
    fn the_audit_and_offload_routes_are_local_capability_at_the_gate() {
        // An EXTERNAL-latched, contaminated conversation refuses all four.
        for blocked in [
            "security_audit",
            "quality_audit",
            "offload_task",
            "offload_batch",
        ] {
            let reg = LatchRegistry::default();
            let s = scope("claude-1", Some("sess-a"));
            assert!(reg
                .gate(
                    Some(&s),
                    LatchRoute::Proxied,
                    "ddg__fetch_content",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Native, blocked, ON, NO_CONTENT),
                Err(REFUSAL_LOCAL_BLOCKED),
                "{blocked} must be refused once the conversation has read a page"
            );
        }
        // …and in the other direction each of them LATCHES, closing the web for
        // the rest of the session. That is the accepted consequence of the
        // split, so it is asserted rather than discovered in the field.
        for first in [
            "security_audit",
            "quality_audit",
            "offload_task",
            "offload_batch",
        ] {
            let reg = LatchRegistry::default();
            let s = scope("claude-1", Some("sess-a"));
            assert!(reg
                .gate(Some(&s), LatchRoute::Native, first, ON, NO_CONTENT)
                .is_ok());
            assert_eq!(reg.snapshot()[0].latch(), "local", "{first}");
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
                Err(REFUSAL_EXTERNAL_BLOCKED),
                "{first}"
            );
        }
        // The `/run` body's `tool` field is a LABEL, never a capability: only
        // the two real names survive the parse boundary, and both classify the
        // same, so no value a caller invents can change the verdict above.
        assert_eq!(offload_tool_name(Some("offload_batch")), "offload_batch");
        assert_eq!(offload_tool_name(Some(" offload_batch ")), "offload_batch");
        for raw in [None, Some(""), Some("offload_task"), Some("graph_outline")] {
            assert_eq!(offload_tool_name(raw), "offload_task", "{raw:?}");
        }
        // The `/audit/run` gate's name comes from the category, through the one
        // mapping the child's `tools/call` also uses.
        assert_eq!(
            crate::audit::mcp::tool_name_for(crate::audit::adapters::Category::Security),
            "security_audit"
        );
        assert_eq!(
            crate::audit::mcp::tool_name_for(crate::audit::adapters::Category::Quality),
            "quality_audit"
        );
    }

    // ── H-8 (2026-08-08 re-review): `/audit/run`'s gate is not opt-in ──────
    //
    // The finding: the gate's only identity input was `body.tab`, caller
    // supplied and optional. Absent ⇒ `LatchScoping::Anonymous` ⇒ `scope()`
    // `None` ⇒ `gate()` returned `Ok(Clean)` before classifying anything, and
    // said nothing about it. Compounding, `consumer` was caller-asserted and
    // unbounded while selecting which `expose_*` toggle was checked — including
    // `"offload"`, which defaults true and which no legitimate caller sends.
    //
    // These drive [`audit_admit`], which is the route's ENTIRE pre-scan
    // decision (the handler adds only body parsing, state resolution and the
    // wire framing), so the ordering they assert is the ordering that ships.

    /// A `/audit/run` body. `Security` throughout: the gate's tool name comes
    /// from the category and both categories classify identically
    /// (`the_audit_and_offload_routes_are_local_capability_at_the_gate`).
    fn audit_body(consumer: Option<&str>, tab: Option<&str>) -> AuditRunBody {
        AuditRunBody {
            category: crate::audit::adapters::Category::Security,
            consumer: consumer.map(str::to_string),
            cwd: None,
            tab: tab.map(str::to_string),
        }
    }

    /// Drive [`audit_admit`] against `reg` with a fixed served root, an
    /// `exposed` verdict and a pre-resolved scoping — the same three
    /// dependencies the handler supplies from `AuditState` / `latch_scope` /
    /// `GatePolicy::resolve`.
    fn admit(
        reg: &LatchRegistry,
        body: &AuditRunBody,
        exposed: bool,
        scoping: LatchScoping,
    ) -> Result<&'static str, String> {
        audit_admit(
            reg,
            body,
            Path::new("P:\\proj"),
            |_| exposed,
            |_, _| scoping,
            |_| ON,
        )
    }

    /// H-8, half 1. A body with no usable tab identity is REFUSED — and the
    /// refusal engages nothing, because it happens before any `LatchScope`
    /// exists. The message names the remedy (restart the tab), because the only
    /// legitimate way to arrive here is a child left over from a pre-C-1b
    /// build.
    #[test]
    fn audit_run_refuses_a_body_with_no_tab_and_engages_no_latch() {
        for tab in [None, Some(""), Some("   "), Some("\t")] {
            let reg = LatchRegistry::default();
            let err = admit(
                &reg,
                &audit_body(Some("claude"), tab),
                true,
                // Unreachable: the refusal precedes scope resolution. Anything
                // here would be a scope the refusal must not have used.
                LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
            )
            .expect_err("a body with no tab identity must be refused");
            assert!(
                err.contains("restart this tab"),
                "the refusal must name the remedy, got {err:?}"
            );
            // The invariant, not the string: a refused request leaves the
            // registry exactly as it found it.
            assert!(
                reg.snapshot().is_empty(),
                "a refused request must not key a latch row ({tab:?})"
            );
        }
    }

    /// H-8, half 1 — the exploit, re-run. An EXTERNAL-latched (contaminated)
    /// conversation that curls the route *with a tab* is refused by the gate;
    /// the same conversation curling it *without* one — which used to return the
    /// full gitleaks report while consulting no latch at all — is refused too.
    #[test]
    fn audit_run_refuses_a_contaminated_tab_with_or_without_an_id() {
        let reg = LatchRegistry::default();
        // Contaminate: one proxied fetch closes the local side for the session.
        assert!(reg
            .gate(
                Some(&scope("claude-1", Some("sess-a"))),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "external");

        // With its own identity: the gate now actually runs, and refuses.
        let err = admit(
            &reg,
            &audit_body(Some("claude"), Some("claude-1")),
            true,
            LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
        )
        .expect_err("a contaminated conversation must not run a local scanner");
        assert_eq!(err, REFUSAL_LOCAL_BLOCKED);

        // Dropping `tab` was the whole exploit — it is no longer an escape.
        let err = admit(
            &reg,
            &audit_body(Some("claude"), None),
            true,
            LatchScoping::Anonymous,
        )
        .expect_err("omitting `tab` must not opt the caller out of the gate");
        assert!(err.contains("restart this tab"), "{err:?}");

        // Neither refusal moved the latch (a refused call must never redefine
        // which side of the boundary the session is on).
        assert_eq!(reg.snapshot()[0].latch(), "external");
    }

    /// H-8, half 1 — the surviving no-scope path. An id naming no configured
    /// tab keeps #45's behaviour: no registry row, no refusal (fail-open on a
    /// TOOL route), and — this is the H-8 half — it is WARNED rather than
    /// silent. The warn is written over `scope().is_none()`, so it covers
    /// `Anonymous` too if step 4 ever regresses; that predicate is pinned here
    /// because the log line itself is not observable from a unit test.
    #[test]
    fn audit_run_warns_but_still_runs_for_an_unknown_tab() {
        let reg = LatchRegistry::default();
        assert_eq!(
            admit(
                &reg,
                &audit_body(Some("claude"), Some("ghost")),
                true,
                LatchScoping::Unknown("ghost".into()),
            ),
            Ok("claude")
        );
        assert!(
            reg.snapshot().is_empty(),
            "#45's bound: an unknown id keys no registry entry"
        );
        // Both identity-less variants take the warn branch.
        assert!(LatchScoping::Unknown("ghost".into()).scope().is_none());
        assert!(LatchScoping::Anonymous.scope().is_none());
    }

    /// H-8 — containment must not be bought by breaking the route. A clean,
    /// configured tab is admitted, and the scan engages that tab's latch (which
    /// is also what proves the refusal tests above are asserting a registry the
    /// success path really does write to).
    #[test]
    fn audit_run_admits_a_clean_configured_tab_and_engages_its_latch() {
        for (consumer, expect) in [
            (None, "claude"),
            (Some("claude"), "claude"),
            (Some("opencode"), "opencode"),
            (Some(" OpenCode "), "opencode"),
        ] {
            let reg = LatchRegistry::default();
            assert_eq!(
                admit(
                    &reg,
                    &audit_body(consumer, Some("claude-1")),
                    true,
                    LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
                ),
                Ok(expect),
                "{consumer:?}"
            );
            assert_eq!(
                reg.snapshot()[0].latch(),
                "local",
                "an admitted LOCAL-CAPABILITY scan closes the web side"
            );
        }
    }

    /// H-8, half 2. `consumer` is narrowed to the two consumers that actually
    /// exist before it can select an `expose_*` toggle.
    ///
    /// `"offload"` is the one that mattered: `AuditState::consumer_exposed`
    /// maps it to `expose_offload`, which **defaults true**, while
    /// `graph::source_for_consumer` maps it to `"claude"` — so a forged caller
    /// passed a toggle no legitimate caller uses and latched as somebody else.
    /// The `exposed` closure panics here, which is how the test proves no
    /// toggle is selected at all rather than merely that the request failed.
    #[test]
    fn audit_run_rejects_a_consumer_outside_the_legitimate_set() {
        for bad in ["offload", "worker", "OFFLOAD", "claude ext", "clau de", "x"] {
            let reg = LatchRegistry::default();
            let body = audit_body(Some(bad), Some("claude-1"));
            let err = match audit_admit(
                &reg,
                &body,
                Path::new("P:\\proj"),
                |c| panic!("an expose toggle was selected for the rejected consumer {c:?}"),
                |_, _| panic!("identity was resolved for a rejected consumer"),
                |_| ON,
            ) {
                Ok(c) => panic!("{bad:?} must not be accepted as a consumer (got {c:?})"),
                Err(e) => e,
            };
            assert!(
                err.contains("does not serve the consumer"),
                "{err:?} ({bad})"
            );
            assert!(reg.snapshot().is_empty(), "{bad}");
        }
        // The set itself, and the two spellings the spawn paths actually send.
        assert_eq!(AUDIT_CONSUMERS, ["claude", "opencode"]);
        assert_eq!(audit_consumer(None), Ok("claude"));
        assert_eq!(audit_consumer(Some("")), Ok("claude"));
        assert_eq!(audit_consumer(Some("  ")), Ok("claude"));
        assert_eq!(audit_consumer(Some("CLAUDE")), Ok("claude"));
        assert_eq!(audit_consumer(Some(" opencode ")), Ok("opencode"));
        // …and the value that reaches `consumer_exposed` is one of those two
        // literals, never the caller's string, so no `expose_*` toggle outside
        // the pair is reachable over HTTP.
        for c in AUDIT_CONSUMERS {
            assert_eq!(audit_consumer(Some(c)), Ok(c));
        }
    }

    /// H-8 — ordering. The two pre-existing refusals still come first (their
    /// messages are the actionable ones), and neither leaves latch state
    /// behind: a request that was never going to run must not engage the tab's
    /// latch. Same registry the success path above writes to, so an empty
    /// snapshot here is a real observation.
    #[test]
    fn audit_run_refusals_before_the_gate_leave_no_latch_state() {
        // Not exposed — refused before identity is even resolved.
        let reg = LatchRegistry::default();
        let err = admit(
            &reg,
            &audit_body(Some("opencode"), Some("opencode")),
            false,
            LatchScoping::Scoped(scope("opencode", Some("sess-a"))),
        )
        .expect_err("an opted-out consumer must be refused");
        assert!(err.contains("is not exposed to opencode"), "{err:?}");
        assert!(
            reg.snapshot().is_empty(),
            "expose refusal keyed a latch row"
        );

        // Misrouted (cwd outside this instance's served root) — likewise.
        let reg = LatchRegistry::default();
        let mut body = audit_body(Some("claude"), Some("claude-1"));
        body.cwd = Some("P:\\other-project".into());
        let err = audit_admit(
            &reg,
            &body,
            Path::new("P:\\proj"),
            |_| true,
            |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
            |_| ON,
        )
        .expect_err("a misrouted child must be refused");
        assert!(err.contains("this cImp instance serves"), "{err:?}");
        assert!(reg.snapshot().is_empty(), "cwd refusal keyed a latch row");
    }

    /// The locked cross-module invariant, through the proxy: a server nobody
    /// has classified is EXTERNAL, so calling it latches the session exactly
    /// like `ddg__*` does.
    #[test]
    fn an_unknown_proxied_server_latches_as_external() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "somenewserver__anything",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "external");
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
    }

    /// Locked decision 10, as built in Phase C2: a memory write under an
    /// EXTERNAL latch is **quarantined, not refused** — the note is stored with
    /// a `tainted` flag and withheld from every read path, so an injected page
    /// still cannot plant a note that auto-injects into future clean sessions,
    /// but a legitimate research conclusion is preserved for review instead of
    /// being thrown away (the Phase A/B behaviour).
    #[test]
    fn context_note_is_quarantined_under_an_external_latch_only() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        // Unlatched: clean, and the write itself does not latch.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean)
        );
        assert_eq!(reg.snapshot()[0].latch(), "open");

        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        // EXTERNAL-latched: proceeds, tainted — NOT `Err(REFUSAL_WRITE_BLOCKED)`.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined)
        );
        // ...and the quarantined write still does not move the latch.
        assert_eq!(reg.snapshot()[0].latch(), "external");
        // Reads of the same store stay open — quarantine is about persistence.
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "context_recall",
                ON,
                NO_CONTENT
            ),
            Ok(WriteTaint::Clean)
        );
    }

    /// The other direction of the same rule: a LOCAL-CAPABILITY latch never
    /// taints a write — only external content can contaminate persistence.
    #[test]
    fn a_local_latch_writes_clean() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean)
        );
    }

    /// #48 F-20 — the three-way mapping, pinned to concrete values.
    ///
    /// [`LatchScoping`] and [`crate::activity::Attribution`] were derived from the
    /// same three facts, and the row's column exists to say which of the three a
    /// call was. The `match` below is exhaustive on purpose: a fourth variant has
    /// to be given a reading here rather than silently inheriting one.
    #[test]
    fn latch_scoping_maps_onto_exactly_one_attribution_state_each() {
        use crate::activity::Attribution;
        assert_eq!(LatchScoping::Anonymous.attribution(), Attribution::Headless);
        assert_eq!(
            LatchScoping::Unknown("ghost".to_string()).attribution(),
            Attribution::Unrecognized("ghost".to_string())
        );
        assert_eq!(
            LatchScoping::Scoped(scope("claude-1", Some("sess-a"))).attribution(),
            Attribution::Tab("claude-1".to_string())
        );
        for s in [
            LatchScoping::Anonymous,
            LatchScoping::Unknown("ghost".to_string()),
            LatchScoping::Scoped(scope("claude-1", None)),
        ] {
            // Exhaustiveness: the compiler is the enumeration guard.
            let _: () = match &s {
                LatchScoping::Anonymous | LatchScoping::Unknown(_) | LatchScoping::Scoped(_) => (),
            };
            assert_ne!(
                s.attribution(),
                Attribution::Unattributed,
                "a route that resolved a scoping DOES know which of the three this was"
            );
        }
    }

    /// The case the whole finding is about: `Anonymous` and `Unknown` are ONE
    /// `None` to the latch — correctly, both fail open — and must be TWO states
    /// on the row.
    #[test]
    fn an_unrecognized_tab_id_is_never_reported_as_headless() {
        use crate::activity::Attribution;
        let ghost = || LatchScoping::Unknown("not-a-real-tab".to_string());
        // The collapse that is right for the latch…
        assert!(
            ghost().into_scope().is_none(),
            "#45's bound: an unrecognized id keys no registry entry"
        );
        assert!(LatchScoping::Anonymous.into_scope().is_none());
        // …and wrong for the row.
        assert_ne!(ghost().attribution(), Attribution::Headless);
        assert_ne!(
            ghost().attribution(),
            LatchScoping::Anonymous.attribution(),
            "F-20: these two were one `None` and must be two row states"
        );
    }

    /// …and an unrecognized id is never reported as a real tab either — the rule
    /// `activity::tests::only_a_configured_tab_counts_as_a_tab` states, from the
    /// producer side.
    #[test]
    fn an_unrecognized_tab_id_is_never_reported_as_a_tab() {
        let attr = LatchScoping::Unknown("not-a-real-tab".to_string()).attribution();
        assert!(
            !attr.is_tab(),
            "filtering by a tab id must never surface a row that merely quoted it"
        );
        assert_eq!(attr.id(), Some("not-a-real-tab"));
    }

    /// **#48 F-39 / locked decision 42 — an invented tab id cannot choose how
    /// many bytes of a capped lane one row occupies, and truncating it is not a
    /// way to become a real tab.**
    ///
    /// Three halves, and the ORDER is the finding's subtle part.
    ///
    /// 1. The bound itself, on the row the producer actually writes
    ///    (`attribution()`, which `/graph_run` and `/mcp/call` both call).
    /// 2. **Classification sees the FULL string.** A body id that is a configured
    ///    tab id plus a suffix — so that a naive parse-boundary truncation would
    ///    hand `is_configured_tab` the configured id — must still resolve as
    ///    `Unknown`. This is the assertion that fails if a future "fix" moves
    ///    `bounded_id` earlier, closing the bloat hole by opening an
    ///    impersonation one.
    /// 3. The truncated id is still not a configured tab, from the same
    ///    `is_configured_tab` the resolution used.
    ///
    /// **What this would still pass if the implementation were wrong:** it would
    /// pass a bound applied anywhere at or after `tab_identity` (the constructor
    /// in `latch_scope`, say) — deliberately, because every such site is after
    /// classification and any of them is correct. It would NOT pass a bound
    /// applied to `body.tab` before the identity check, and it would not pass a
    /// larger-than-`BEACON_TOOL_MAX` bound, an ellipsis-free truncation that
    /// happened to equal a configured id, or no bound at all.
    #[test]
    fn an_invented_tab_id_is_bounded_before_it_reaches_a_row_and_after_it_is_classified() {
        use crate::activity::Attribution;
        // A configured id exactly as long as the bound, so a truncation applied
        // one step too early would produce this very string.
        let real = "t".repeat(BEACON_TOOL_MAX);
        let s = settings_with_tabs(&[real.as_str()]);
        let forged = format!("{real}-and-then-some{}", "x".repeat(4096));

        // (2) The classifier is handed the whole thing, so the suffix counts.
        assert!(
            matches!(
                tab_identity(&s, "claude", Some(forged.as_str())),
                TabIdentity::Unknown(_)
            ),
            "truncation must not run before `is_configured_tab`"
        );
        assert!(matches!(
            tab_identity(&s, "claude", Some(real.as_str())),
            TabIdentity::Configured(_)
        ));

        // (1) The row's attribution is bounded — chars, not bytes, and one
        // ellipsis says it was cut.
        let attr = LatchScoping::Unknown(forged.clone()).attribution();
        let Attribution::Unrecognized(id) = &attr else {
            panic!("a 4 KiB invented id is not a tab: {attr:?}");
        };
        assert!(
            id.chars().count() <= BEACON_TOOL_MAX + 1,
            "{} chars reached the row",
            id.chars().count()
        );
        assert!(id.ends_with('…'), "a cut id must say it was cut: {id}");

        // (3) …and it is still nobody's tab.
        assert!(
            !is_configured_tab(&s, "claude", id),
            "truncation is not a forgery"
        );
        assert_ne!(*id, real);
        assert!(!attr.is_tab());

        // A multi-byte id is cut on a codepoint, never mid-character.
        let wide = LatchScoping::Unknown("é".repeat(4096)).attribution();
        let Attribution::Unrecognized(id) = &wide else {
            panic!("not a tab: {wide:?}");
        };
        assert!(id.chars().count() <= BEACON_TOOL_MAX + 1);

        // An id that fits is untouched — the bound must not cost the honest case
        // anything, since a stale-but-real id is why this variant exists.
        assert_eq!(
            LatchScoping::Unknown("opencode-removed".to_string()).attribution(),
            Attribution::Unrecognized("opencode-removed".to_string())
        );
    }

    /// #48 F-16 — the unattributed-write row names the project it was about to
    /// write into.
    ///
    /// [`LatchRegistry::gate`] has no scope to take a root from — that is
    /// [`unattributed_write`]'s whole premise — but the ROUTE does (`/graph_run`
    /// holds `body.cwd`, resolved through `GraphService::graph_root_key`, the same
    /// resolution the dispatch's own `kind:"graph"` row uses). Before this the row
    /// carried `root: ""`, so a project-scoped review could not see it.
    #[test]
    fn an_unattributed_write_row_names_the_project_it_was_about_to_write_into() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        assert_eq!(
            reg.gate(
                None,
                LatchRoute::Native,
                "context_note",
                ON,
                CallProvenance::internal_in(TEST_ROOT),
            ),
            Ok(WriteTaint::Unattributed)
        );
        let rows = outbound::test_rows::drain();
        let held = outbound::test_rows::of_screen(&rows, outbound::Screen::MemoryQuarantine);
        assert_eq!(held.len(), 1, "one held note, one review-queue row");
        assert_eq!(held[0].entry.root, TEST_ROOT);
        assert!(!held[0].entry.root.is_empty());
    }

    /// #48 (2026-08-08 re-review), M-19 — the identity-less PERSISTENT-WRITE.
    ///
    /// This case used to be the tail of the test above, asserting
    /// `Ok(WriteTaint::Clean)` under the comment *"no tab identity ⇒ no scope to
    /// latch and none to taint"*. The first half of that is locked (F-5/H-8) and
    /// still holds; the second half was the defect — a note nobody could
    /// attribute, stored as ordinary auto-injecting memory, while the headless
    /// path refuses the very same call for the very same missing facts.
    ///
    /// Three properties, and dropping any one of them re-opens something:
    /// the write is HELD (not clean, and not refused — locked decision 10);
    /// it is held as `Unattributed`, so the model gets the true reason rather
    /// than a claim about external content; and it still creates no latch row,
    /// so the fail-open the fix is *not* touching stays untouched.
    #[test]
    fn an_identityless_persistent_write_is_held_not_stored_clean() {
        let reg = LatchRegistry::default();
        assert_eq!(
            reg.gate(
                None,
                LatchRoute::Native,
                "context_note",
                ON,
                NATIVE_IN_PROJECT
            ),
            Ok(WriteTaint::Unattributed)
        );
        assert!(WriteTaint::Unattributed.is_quarantined());
        assert_eq!(
            WriteTaint::Unattributed.write_notice(),
            Some(toolclass::UNATTRIBUTED_WRITE_NOTICE),
            "and it is explained as itself, not as an external-content quarantine"
        );
        assert!(
            reg.snapshot().is_empty(),
            "an identityless call still creates no latch row"
        );

        // Locked decision 16: this is a QUARANTINE decision, so the quarantine
        // switch turns it off — and the latch switch does not. Without the
        // second assertion the feature switch could be wired to the wrong half
        // and nothing would notice.
        let latch_only = GatePolicy {
            latch: true,
            quarantine: false,
        };
        assert_eq!(
            reg.gate(
                None,
                LatchRoute::Native,
                "context_note",
                latch_only,
                NO_CONTENT
            ),
            Ok(WriteTaint::Clean)
        );
        let quarantine_only = GatePolicy {
            latch: false,
            quarantine: true,
        };
        assert_eq!(
            reg.gate(
                None,
                LatchRoute::Native,
                "context_note",
                quarantine_only,
                NATIVE_IN_PROJECT
            ),
            Ok(WriteTaint::Unattributed)
        );
    }

    // ── V32 Phase H — the OpenCode native-tool gate's backend half ─────────

    /// An OpenCode scope for `tab`.
    fn oc_scope(tab: &str, session: Option<&str>) -> LatchScope {
        LatchScope {
            agent: "opencode",
            tab: tab.to_string(),
            session: session.map(str::to_string),
            root: TEST_ROOT.to_string(),
        }
    }

    /// Settings carrying the builtin OpenCode tab, so a per-tab L3 cell has a
    /// tab to attach to (`Settings::default()` ships an EMPTY tab list).
    fn oc_settings() -> (crate::settings::Settings, String) {
        let tab = match crate::settings::default_opencode_tab() {
            crate::settings::TabConfig::AiTool(c) => c,
            _ => unreachable!("default_opencode_tab is an AI tool tab"),
        };
        let id = tab.id.clone();
        (
            crate::settings::Settings {
                tabs: vec![crate::settings::TabConfig::AiTool(tab)],
                ..Default::default()
            },
            id,
        )
    }

    /// The verdict the plugin is handed: **off by default**, on only when the
    /// Phase H feature AND the taint latch both resolve on, and off again the
    /// moment the master switch goes.
    #[test]
    fn the_native_gate_verdict_is_off_by_default_and_needs_the_latch_too() {
        use crate::settings::injection::{Feature, Override};
        let (mut s, id) = oc_settings();
        let scope = oc_scope(&id, Some("ses"));
        assert!(
            !native_gate_verdict(&s, scope.injection()),
            "locked decision 17: the gate ships OFF"
        );

        // The app-wide L2.
        s.set_l2_for_test(Feature::OpencodeNativeGate, true);
        assert!(native_gate_verdict(&s, scope.injection()));

        // The taint latch is what this gate enforces — with that feature off
        // there is no boundary to enforce, so the gate reports off LIVE (no tab
        // restart), even though its own flag stays baked in the plugin.
        s.set_l2_for_test(Feature::TaintLatch, false);
        assert!(!native_gate_verdict(&s, scope.injection()));
        s.set_l2_for_test(Feature::TaintLatch, true);

        // The usual way in: L2 off app-wide, one tab's L3 `On`.
        s.set_l2_for_test(Feature::OpencodeNativeGate, false);
        s.set_tab_override_for_test(&id, Feature::OpencodeNativeGate, Override::On)
            .expect("the OpenCode tab carries a native-gate cell");
        assert!(
            native_gate_verdict(&s, scope.injection()),
            "an L3 On enables one tab"
        );
        assert!(
            !native_gate_verdict(&s, oc_scope("some-other-tab", Some("ses")).injection()),
            "and only that tab"
        );

        // Nothing re-enables past the master.
        s.set_master_for_test(false);
        assert!(!native_gate_verdict(&s, scope.injection()));
    }

    /// **#48 (A2-1): a tab id the settings no longer carry is not a hard OFF.**
    ///
    /// #45 folded "not a configured tab" into `latch_scope`'s `None`, and
    /// `handle_latch_state` mapped that `None` to `(false, default)` — so the
    /// Phase H gate reported OFF for an id that had simply gone stale. That is
    /// the ordinary case, not an exotic one: the OpenCode plugin is written per
    /// working *directory* with one tab id baked in (the unfixed H-2), so
    /// removing or re-id'ing a tab leaves the file naming an id settings no
    /// longer have — and "the user switched containment off" and "cImp could
    /// not find your tab" then rendered identically to the plugin.
    ///
    /// The verdict now follows the resolved scope, which is the unknown
    /// caller's for both identity-less shapes. Asserted as the *equality* the
    /// fix is about: an unknown id answers what an unattributed call answers,
    /// whatever that is.
    ///
    /// **Renamed with #48 F-35** (was
    /// `…_resolves_the_app_wide_gate_verdict_…`): locked decision 36 split
    /// `Scope::App` into `Scope::AppWide` and `Scope::UnknownCaller`, and this
    /// test asserts the second one. "App-wide" stopped describing it — the
    /// resolved answer here also carries any configured tab's L3 `On` (N-1),
    /// which the app-wide baseline does not.
    #[test]
    fn an_unknown_tab_id_resolves_as_an_unknown_caller_not_a_hard_off() {
        use crate::settings::injection::{Feature, Scope};
        let (mut s, _id) = oc_settings();
        let stale = LatchScoping::Unknown("opencode-removed".to_string());
        let anon = LatchScoping::Anonymous;
        assert!(matches!(stale.injection(), Scope::UnknownCaller));
        assert!(matches!(anon.injection(), Scope::UnknownCaller));

        // Off app-wide ⇒ off for a stale id. (The regression was invisible in
        // this direction, which is why #45 shipped.)
        assert!(!native_gate_verdict(&s, stale.injection()));

        // ON app-wide ⇒ ON for a stale id. This is the assertion that fails if
        // the hard-off comes back.
        s.set_l2_for_test(Feature::OpencodeNativeGate, true);
        assert!(
            native_gate_verdict(&s, stale.injection()),
            "a stale tab id must inherit the app-wide verdict, not report off"
        );
        assert_eq!(
            native_gate_verdict(&s, stale.injection()),
            native_gate_verdict(&s, Scope::UnknownCaller),
            "and it must be the SAME answer an unattributed call gives, by construction"
        );

        // Through the reply the plugin actually reads, which is where the
        // regression lived: a `match` arm mapping "no usable identity" to a
        // hard-off verdict. The `latch` stays `open` because an unknown id keys
        // no registry entry — that part is #45's bound and is deliberate.
        let reply = latch_state_reply(&s, &stale, LatchView::default());
        assert_eq!(reply["gate"], true, "{reply}");
        assert_eq!(reply["latch"], "open", "{reply}");
        assert_eq!(reply["contaminated"], false, "{reply}");
        assert_eq!(
            latch_state_reply(&s, &anon, LatchView::default())["gate"],
            true,
            "an identity-less body resolves the same app-wide verdict"
        );

        // #45's actual goal is untouched: an unusable id yields no scope, so
        // nothing can key a registry entry off it.
        assert!(stale.scope().is_none());
        assert!(anon.scope().is_none());
        assert!(stale.into_scope().is_none());

        // The latch still ANDs in, live — a stale id cannot resurrect a gate
        // whose boundary nobody is maintaining.
        s.set_l2_for_test(Feature::TaintLatch, false);
        assert!(!native_gate_verdict(
            &s,
            LatchScoping::Unknown("x".into()).injection()
        ));
    }

    /// #48 (A2-6): `/latch/beacon`'s `tool` is an arbitrary unbounded string
    /// from a request body and it lands in an activity row, a `tracing` line
    /// and (through the feed) the TTS surface. Bounded before any of them.
    #[test]
    fn a_beacon_tool_name_is_bounded_before_it_reaches_a_row() {
        assert_eq!(bounded_tool(Some("WebFetch")), "WebFetch");
        assert_eq!(bounded_tool(Some("  webfetch  ")), "webfetch");
        // Absent, empty and whitespace all take the same honest placeholder.
        for empty in [None, Some(""), Some("   ")] {
            assert_eq!(bounded_tool(empty), "(native web tool)", "{empty:?}");
        }
        let long = "A".repeat(5_000);
        let bounded = bounded_tool(Some(&long));
        assert_eq!(bounded.chars().count(), BEACON_TOOL_MAX + 1);
        assert!(bounded.ends_with('…'), "truncation is visible to a reader");
        // Truncated by CHARS: a multi-byte name cannot be cut mid-codepoint,
        // which would panic on a byte slice and produce mojibake in the feed.
        let wide = "→".repeat(200);
        let bounded = bounded_tool(Some(&wide));
        assert_eq!(bounded.chars().count(), BEACON_TOOL_MAX + 1);
        assert!(bounded.starts_with('→'));
        // Exactly at the bound: no ellipsis, nothing lost.
        let exact = "b".repeat(BEACON_TOOL_MAX);
        assert_eq!(bounded_tool(Some(&exact)), exact);
    }

    /// `view_for` is the gate's read path: it must answer for a tab the proxy
    /// has never served WITHOUT creating a row (a poll is not a tool call), and
    /// the answer must be the one that denies nothing.
    #[test]
    fn view_for_answers_open_for_an_unknown_tab_without_creating_a_row() {
        let reg = LatchRegistry::default();
        let view = reg.view_for(&oc_scope("never-served", Some("ses")));
        assert_eq!(view, LatchView::default());
        assert_eq!(view.latch, "open", "fail-open: nothing to deny against");
        assert!(
            reg.snapshot().is_empty(),
            "a state read must not materialize a latch row"
        );
    }

    /// The read path reports the live latch — including after the decision-15
    /// override, which is what makes "switch to local" move the native gate with
    /// it (locked decision 17's last sentence) — and it rotates a stale latch
    /// with the session, so a fresh conversation is never denied `read`/`bash`
    /// on the strength of the previous one's fetch.
    #[test]
    fn view_for_tracks_the_latch_including_overrides_and_session_rotation() {
        let reg = LatchRegistry::default();
        let s = oc_scope("opencode", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        // EXTERNAL ⇒ the plugin denies the local natives.
        let view = reg.view_for(&s);
        assert_eq!(view.latch, "external");
        assert!(view.contaminated);

        // Decision 15's workflow button flips the boundary; the gate follows,
        // because it reads live state rather than caching a verdict.
        reg.apply_override(&s, LatchOverride::FlipLocal).unwrap();
        let view = reg.view_for(&s);
        assert_eq!(view.latch, "local", "the web side is now the denied one");
        assert!(view.contaminated, "an override never un-reads a page");

        // A tab restart rotates the session, and the read path sees it — a
        // stale `external` here would deny the whole local surface for a fresh
        // conversation.
        let after = oc_scope("opencode", Some("sess-b"));
        assert_eq!(reg.view_for(&after).latch, "open");
    }

    /// Per-tab isolation: one contaminated tab must not disarm (or arm) any
    /// other, and the same tab id under a different agent is a different tab.
    #[test]
    fn latches_are_isolated_per_tab_and_per_agent() {
        let reg = LatchRegistry::default();
        let a = scope("claude-1", Some("sess-a"));
        let b = scope("claude-2", Some("sess-b"));
        let opencode = LatchScope {
            agent: "opencode",
            tab: "claude-1".to_string(),
            session: Some("sess-c".to_string()),
            root: TEST_ROOT.to_string(),
        };

        assert!(reg
            .gate(Some(&a), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&a),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        // Tab B is untouched, and may latch the OTHER way.
        assert!(reg
            .gate(
                Some(&b),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(Some(&b), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        // Same tab STRING, different agent ⇒ its own scope.
        assert!(reg
            .gate(
                Some(&opencode),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());

        let rows = reg.snapshot();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|r| (r.consumer, r.tab.as_str(), r.latch()))
                .collect::<Vec<_>>(),
            [
                ("claude", "claude-1", "external"),
                ("claude", "claude-2", "local"),
                ("opencode", "claude-1", "local"),
            ]
        );
    }

    /// Live-verify 5: a tab restart starts unlatched. The tab id is
    /// config-derived and never rotates, so the reset rides the SESSION id the
    /// V28 registry re-stamps when the new harness session comes up.
    #[test]
    fn a_new_session_for_the_same_tab_starts_unlatched() {
        let reg = LatchRegistry::default();
        let before = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&before),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&before),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );

        // Tab restarted: same tab id, new session.
        let after = scope("claude-1", Some("sess-b"));
        assert!(reg
            .gate(
                Some(&after),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let rows = reg.snapshot();
        assert_eq!(rows.len(), 1, "the restart reuses the tab's row: {rows:?}");
        assert_eq!(rows[0].session.as_deref(), Some("sess-b"));
        assert_eq!(rows[0].latch(), "local");
    }

    /// A withheld session id is absence of evidence, not evidence of a
    /// restart — otherwise an injected model could reset its own latch by
    /// calling until the registry blinked (TTL staleness, the H1 same-root
    /// ambiguity). The latch survives; a later real id adopts the same scope.
    #[test]
    fn a_withheld_session_neither_resets_nor_splits_the_latch() {
        let reg = LatchRegistry::default();
        // Latched before the registry knew any session at all.
        let unknown = scope("claude-1", None);
        assert!(reg
            .gate(
                Some(&unknown),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&unknown),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );

        // The session becomes known: same conversation, so the latch carries.
        let known = scope("claude-1", Some("sess-a"));
        assert_eq!(
            reg.gate(
                Some(&known),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-a"));

        // The registry blinks again: still no reset.
        assert_eq!(
            reg.gate(
                Some(&unknown),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        assert_eq!(
            reg.snapshot()[0].session.as_deref(),
            Some("sess-a"),
            "a withheld id must not erase the known one"
        );
    }

    /// Locked fail-open rule: a call with no tab identity (a child spawned
    /// before `--tab`) is never gated. It is deliberately NOT folded into a
    /// global latch — one identityless call would then latch every consumer.
    /// Its EXTERNAL results are still spotlight-wrapped (that needs no
    /// identity; see `handle_mcp_call`).
    ///
    /// #48 M-19 narrows this to what it always meant: never *refused*, and
    /// never latching. The one PERSISTENT-WRITE is admitted too — and held, see
    /// `an_identityless_persistent_write_is_held_not_stored_clean`. Asserted
    /// per name rather than with `.is_ok()`, because `.is_ok()` is true of
    /// every verdict this function can return and so says nothing about which
    /// one each name got.
    #[test]
    fn an_identityless_call_is_never_gated() {
        let reg = LatchRegistry::default();
        for (route, name, taint) in [
            (LatchRoute::Proxied, "ddg__fetch_content", WriteTaint::Clean),
            (LatchRoute::Native, "graph_snippet", WriteTaint::Clean),
            (LatchRoute::Proxied, "ddg__search", WriteTaint::Clean),
            (LatchRoute::Native, "context_note", WriteTaint::Unattributed),
        ] {
            assert_eq!(
                reg.gate(None, route, name, ON, NATIVE_IN_PROJECT),
                Ok(taint),
                "{name}"
            );
        }
        assert!(
            reg.snapshot().is_empty(),
            "an identityless call must not create a latch row"
        );
        // And it does not leak into a tab that DOES have identity.
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
    }

    /// A refused call must never engage or flip the latch: otherwise a
    /// hallucinated (or injected) call to the blocked side could redefine which
    /// side of the boundary the session is on.
    #[test]
    fn a_refused_call_does_not_move_the_latch() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        for _ in 0..3 {
            assert_eq!(
                reg.gate(
                    Some(&s),
                    LatchRoute::Native,
                    "graph_snippet",
                    ON,
                    NO_CONTENT
                ),
                Err(REFUSAL_LOCAL_BLOCKED)
            );
            assert_eq!(reg.snapshot()[0].latch(), "external");
        }
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "external");
    }

    /// `/graph_run` cannot serve a proxied server's content, so a name that
    /// classifies EXTERNAL there is a typo or a hallucination — `run_graph_tool`
    /// answers "unknown tool". It must not latch the tab: one bad tool name
    /// would otherwise cost the session its local graph tools until restart.
    #[test]
    fn an_unserveable_name_on_the_native_route_does_not_latch_the_tab() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        for junk in ["graph_", "graph_nosuchtool", "ddg__search", ""] {
            assert!(
                reg.gate(Some(&s), LatchRoute::Native, junk, ON, NO_CONTENT)
                    .is_ok(),
                "{junk}"
            );
        }
        assert!(
            reg.snapshot().is_empty(),
            "an unserveable native name must leave the tab unlatched"
        );
        // The real local-capability call that follows still latches normally.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local");
    }

    /// `/status`'s Phase B shape: the `Latch::label()` vocabulary plus the
    /// identity needed to tell whose latch it is. Asserted key-by-key (rather
    /// than as a whole-object equality) so V32 Phase F's additions — which
    /// flatten alongside these — cannot break the guarantee this test exists
    /// for: `latch` stays a TOP-LEVEL key with the three-label vocabulary.
    /// The full Phase F object is pinned by
    /// `status_snapshot_carries_contamination_and_override_availability`.
    #[test]
    fn status_snapshot_serializes_the_latch_labels() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        let json = serde_json::to_value(reg.snapshot()).unwrap();
        let row = &json[0];
        assert_eq!(row["consumer"], "claude");
        assert_eq!(row["tab"], "claude-1");
        assert_eq!(row["session"], "sess-a");
        assert_eq!(row["latch"], "external");
    }

    // ── V32 Phase C — the proxy's per-session EXTERNAL fetch budget ─────────

    const TEST_LIMITS: outbound::BudgetLimits = outbound::BudgetLimits {
        max_calls: 3,
        max_bytes: 1000,
    };

    /// The count half: three proxied calls, then every further one is refused
    /// with the fixed string — and the fourth refusal is the same as the first
    /// (a spent budget does not un-spend).
    #[test]
    fn the_session_budget_stops_a_fetch_loop() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        for _ in 0..3 {
            assert!(reg
                .gate(
                    Some(&s),
                    LatchRoute::Proxied,
                    "ddg__fetch_content",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            assert!(reg
                .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
                .is_ok());
            reg.charge(Some(&s), 10);
        }
        for _ in 0..3 {
            assert_eq!(
                reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
                Err(outbound::REFUSAL_BUDGET)
            );
        }
    }

    /// The byte half, and the fact that it bites on the call AFTER the one
    /// that crossed the cap (a response's size is unknowable beforehand).
    #[test]
    fn the_session_budget_also_counts_bytes() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
            .is_ok());
        reg.charge(Some(&s), 999);
        assert!(reg
            .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
            .is_ok());
        reg.charge(Some(&s), 1);
        assert_eq!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
            Err(outbound::REFUSAL_BUDGET)
        );
    }

    /// #48 (finding D-3) — **a FAILED proxied fetch advances the call
    /// counter.** The charge sat on the `Ok` arm alone, so a loop of fetches
    /// against a host that 500s advanced nothing and never exhausted the
    /// budget: the one screen whose whole purpose is stopping a loop was blind
    /// to the loop that costs least to run. The worker's copy of the same
    /// contract charged both arms (an `Err` there becomes an `ERROR: …` tool
    /// result with `executed = true`), so the two paths disagreed.
    ///
    /// Driven through `charge_call` — the exact function the handler calls, in
    /// one unconditional statement above the match it used to live inside.
    #[test]
    fn a_failed_proxy_fetch_still_advances_the_call_counter() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        let failure: Result<String, String> = Err("upstream 500".into());
        for _ in 0..3 {
            assert!(reg
                .gate(
                    Some(&s),
                    LatchRoute::Proxied,
                    "ddg__fetch_content",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            assert!(reg
                .budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
                .is_ok());
            reg.charge_call(Some(&s), &failure);
        }
        assert_eq!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content"),
            Err(outbound::REFUSAL_BUDGET),
            "three failed fetches must spend the three-call budget"
        );
        // Zero bytes, though: nothing was ingested. The call cap is what stops
        // a loop; the byte cap is about content that arrived.
        let reg = LatchRegistry::default();
        let s = scope("claude-2", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.charge_call(Some(&s), &failure);
        reg.charge_call::<String>(Some(&s), &Ok("x".repeat(999)));
        assert!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__fetch_content")
                .is_ok(),
            "999 bytes is under the 1000-byte cap — the failure contributed none"
        );
    }

    /// #48 — the SSRF denial row is bounded per tab session, and the bound
    /// resets on a proved session rotation.
    ///
    /// Every denial used to write a row with no dedup at all, while the feed
    /// was one 200-row window evicted oldest-first within a kind: a model
    /// looping denied URLs destroyed the `Canary`, `LatchBeacon` and
    /// `MemoryQuarantine` rows that are the only record of an attack that got
    /// through. Finding H-9 closed the cross-screen half of that at the store
    /// (`activity::Lane` — one window per screen, so a loop costs only its own
    /// screen's history); this ledger is what keeps a loop from evicting the
    /// SSRF screen's own first denials. A process-global set keyed on the scope
    /// string was the wrong
    /// shape — proxy scopes are stable `agent:tab`, so it would suppress a
    /// tab's rows across every future session — which is why the ledger rides
    /// the tab's `Budget`.
    #[test]
    fn ssrf_denial_rows_are_bounded_per_session_and_reset_on_rotation() {
        use outbound::DoublingRow;
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        // Drive the registry's own ledger the way `TabAudit` does.
        let claim = || {
            reg.claim(
                Some(&s),
                outbound::Budget::claim_ssrf_flag,
                || DoublingRow::Suppress,
            )
        };
        let written: Vec<u32> = (0..200)
            .filter_map(|_| match claim() {
                DoublingRow::Write { total, .. } => Some(total),
                DoublingRow::Suppress => None,
            })
            .collect();
        assert_eq!(
            written,
            vec![1, 2, 4, 8, 16, 32, 64, 128],
            "200 denials cost the capped feed 8 rows, not 200"
        );
        // The first denial still reports immediately — a single one behaves
        // exactly as it always did.
        let fresh = LatchRegistry::default();
        let f = scope("claude-2", Some("sess-a"));
        assert!(fresh
            .gate(Some(&f), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert!(matches!(
            fresh.claim(
                Some(&f),
                outbound::Budget::claim_ssrf_flag,
                || DoublingRow::Suppress
            ),
            DoublingRow::Write { total: 1, .. }
        ));

        // A new conversation is entitled to its own rows: the rotation that
        // resets the budget resets the ledger with it.
        let rotated = scope("claude-1", Some("sess-b"));
        assert!(reg
            .gate(
                Some(&rotated),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(matches!(
            reg.claim(
                Some(&rotated),
                outbound::Budget::claim_ssrf_flag,
                || DoublingRow::Suppress
            ),
            DoublingRow::Write { total: 1, .. }
        ));

    }

    /// #48 finding F-40 — the identity-less scope is LEDGERED, and this is the
    /// split half of the assertion that used to live at the end of
    /// `ssrf_denial_rows_are_bounded_per_session_and_reset_on_rotation`.
    ///
    /// **Do not merge it back.** The old assertion was
    /// `matches!(TabAudit(None).claim_ssrf(), Write { .. })` under the comment
    /// *"it reports — the same fail-open the latch and the budget take"*, and it
    /// stayed green for exactly the behaviour F-40 measured in the field: a
    /// caller with no tab wrote **one row per denial** (~72 denials → ~64 rows)
    /// where an attributed scope wrote `log2(n)` (20 → 4). Both halves of that
    /// old line are still asserted here — it still reports, and it is now
    /// bounded — but as two claims, so loosening one cannot hide behind the other.
    ///
    /// Assertions are stated as **bounds, not exact totals**, because the ledger
    /// is process-global by design ([`outbound::UnscopedAudit`]): any other test
    /// in this binary that claims unscoped shifts the starting point, and a test
    /// that would fail when its neighbours run is worse than no test.
    #[test]
    fn an_identity_less_call_reports_but_is_still_ledgered() {
        use outbound::{DoublingRow, ScopeAudit};

        // `gate` has never run for this one, so it takes the no-entry path —
        // the same fallback an absent, unknown or shell `tab` reaches.
        let unscoped = TabAudit(None, "claude");

        // It still REPORTS: a lone denial behaves as it always did.
        let first = unscoped.claim_ssrf();
        assert!(
            matches!(first, DoublingRow::Write { .. }),
            "an identity-less denial must still be able to write a row: {first:?}"
        );

        // It is now LEDGERED. `total: 0` was the wire-visible signature of the
        // unledgered fallback this finding removed, and it is what
        // `ssrf_flag_detail` would have to render as "denial #0".
        for _ in 0..64 {
            if let DoublingRow::Write { total, .. } = unscoped.claim_ssrf() {
                assert!(total >= 1, "a written row must count itself");
            }
        }

        // And it is BOUNDED: 128 further denials cost the capped `Ssrf` lane at
        // most a handful of rows, not 128. `log2(128) + 1 = 8` is the ceiling
        // even from a counter starting at zero, so this holds wherever the
        // shared ledger happens to be.
        let written = (0..128)
            .filter(|_| matches!(unscoped.claim_ssrf(), DoublingRow::Write { .. }))
            .count();
        assert!(
            written <= 8,
            "128 identity-less denials wrote {written} rows; the doubling bounds it to 8"
        );

        // The unscreened bit is a hard one-per-scope claim, not a doubling, and
        // the identity-less scope is one scope — so across many calls it is
        // claimable at most once. (It may already be spent by an earlier test;
        // "never twice" is the property, and it is the one that matters.)
        let claims = (0..16).filter(|_| unscoped.claim_unscreened()).count();
        assert!(
            claims <= 1,
            "the one unscreened row per scope was claimed {claims} times"
        );
    }

    /// #48 (finding A-1, proxy side) — restated as the shared rule the worker
    /// now uses too. A bare name that classifies EXTERNAL is a hallucination,
    /// and every proxied id contains `__` by construction, so the restrictive
    /// unknown-⇒-EXTERNAL default still governs every name that can carry
    /// external content.
    #[test]
    fn the_route_rule_is_one_definition_shared_with_the_worker() {
        assert_eq!(LatchRoute::of_tool("graph_symbols"), LatchRoute::Native);
        assert_eq!(LatchRoute::of_tool("read_file"), LatchRoute::Native);
        assert_eq!(LatchRoute::of_tool("ddg__search"), LatchRoute::Proxied);
        assert_eq!(
            LatchRoute::of_tool("somenewserver__anything"),
            LatchRoute::Proxied
        );
        assert!(LatchRoute::Proxied.external_is_content());
        assert!(!LatchRoute::Native.external_is_content());
    }

    /// **#48 (finding M-2) — `can_execute`, the rule A-1 and M-2 share, and the
    /// two ways it must NOT over-reach.**
    ///
    /// The whole risk of widening the wave-through set is that it stops being
    /// about names that cannot run. All three variants are asserted here, and
    /// the `Hook` row is the one that matters most: the three hook names are
    /// exactly the `unrouted` rows, and applying M-2's rule to their own route
    /// would wave through the gate M-7 built.
    #[test]
    fn can_execute_covers_the_unroutable_names_without_reaching_the_hook_routes() {
        let cls = toolclass::classify;
        // Native: a real tool executes; a typo and an unroutable classified
        // name do not.
        for real in [
            "read_file",
            "graph_snippet",
            "context_note",
            "graph_outline",
        ] {
            assert!(
                LatchRoute::Native.can_execute(real, cls(real)),
                "{real} must still be gated"
            );
        }
        for dead in ["graph_symbols", "definitely_not_a_tool", ""] {
            assert!(!LatchRoute::Native.can_execute(dead, cls(dead)), "{dead}");
        }
        for unrouted in ["Bash", "Edit", "Write", "hook_post_edit", "hook_compaction"] {
            assert!(
                !LatchRoute::Native.can_execute(unrouted, cls(unrouted)),
                "{unrouted} reaches no native dispatcher, so it must not move a latch"
            );
        }
        // Hook: the name is cImp's own and IS the route, so M-2's rule must not
        // apply — otherwise `/context/post_edit` stops being refusable and
        // M-7's fix silently unwinds.
        for hook in [
            HOOK_TOOL_POST_EDIT,
            HOOK_TOOL_SHOULD_READ,
            HOOK_TOOL_COMPACTION,
        ] {
            assert!(
                LatchRoute::Hook.can_execute(hook, cls(hook)),
                "{hook} must still be gated on its own route (M-7)"
            );
        }
        // …asserted end-to-end and not just on the predicate: a contaminated
        // tab is still refused `/context/post_edit`.
        let reg = LatchRegistry::default();
        let s = scope("claude-hook", Some("ses"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Hook,
                HOOK_TOOL_POST_EDIT,
                ON,
                NO_CONTENT
            ),
            Err(toolclass::REFUSAL_LOCAL_BLOCKED),
            "M-7: a contaminated conversation must not run the project's checks"
        );
        // …while the same name arriving as a model's tool call is simply not a
        // tool: neither refused nor latching.
        let reg = LatchRegistry::default();
        let s = scope("claude-native", Some("ses"));
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                HOOK_TOOL_POST_EDIT,
                ON,
                NO_CONTENT
            ),
            Ok(WriteTaint::Clean)
        );
        assert!(
            reg.snapshot().is_empty(),
            "a name no dispatcher serves must leave the tab unlatched"
        );
        // Proxied: every id here is a real proxied id, so the rule never
        // applies — an unknown one is untrusted content, not a typo.
        for id in ["ddg__search", "somenewserver__anything"] {
            assert!(LatchRoute::Proxied.can_execute(id, cls(id)), "{id}");
        }
    }

    /// Budgets are scoped exactly like the latch: per tab, and reset when the
    /// tab's SESSION rotates (a tab restart). A withheld session id is not a
    /// rotation — otherwise a model could reset its budget by waiting for the
    /// V28 registry to blink.
    #[test]
    fn the_session_budget_is_per_tab_and_resets_on_session_rotation() {
        let reg = LatchRegistry::default();
        let a = scope("claude-1", Some("sess-a"));
        let b = scope("claude-2", Some("sess-b"));
        for _ in 0..3 {
            assert!(reg
                .gate(Some(&a), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
                .is_ok());
            reg.charge(Some(&a), 1);
        }
        assert_eq!(
            reg.budget_gate(Some(&a), TEST_LIMITS, "ddg__search"),
            Err(outbound::REFUSAL_BUDGET)
        );
        // A different tab is untouched.
        assert!(reg
            .gate(Some(&b), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert!(reg
            .budget_gate(Some(&b), TEST_LIMITS, "ddg__search")
            .is_ok());

        // The registry withholding a session must NOT reset the budget.
        let a_silent = scope("claude-1", None);
        assert!(reg
            .gate(
                Some(&a_silent),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.budget_gate(Some(&a_silent), TEST_LIMITS, "ddg__search"),
            Err(outbound::REFUSAL_BUDGET)
        );

        // A genuinely new session does.
        let a2 = scope("claude-1", Some("sess-a2"));
        assert!(reg
            .gate(
                Some(&a2),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .budget_gate(Some(&a2), TEST_LIMITS, "ddg__search")
            .is_ok());
    }

    // ── V32 Phase F — native-web beacons + the manual override ──────────────

    /// Locked decision 14: a beacon does exactly what an admitted proxied
    /// EXTERNAL call does — engages the tab's latch and contaminates the
    /// conversation — so the harness's own web tool stops being invisible to
    /// containment. The proxied local-capability side closes as a result.
    #[test]
    fn a_native_web_beacon_engages_the_external_latch_like_a_proxied_fetch() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        let view = out.view;
        assert_eq!(view.latch, "external");
        assert!(view.contaminated);
        assert!(view.can_flip_local);
        assert!(view.can_unlatch);
        // #45: the transition is reported, so the handler can write exactly one
        // origin-marked activity row for it.
        assert!(out.engaged, "the beacon MOVED the latch and must say so");
        assert_eq!(reg.snapshot()[0].latch(), "external");
        // ...and the containment that follows is the ordinary Phase B one.
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined)
        );
    }

    /// Fail-open on identity, like every other gate here: a beacon with no tab
    /// id has nothing to engage and must not crash, latch anything globally, or
    /// invent a row. A beacon for a tab the proxy has never served creates that
    /// tab's row, exactly as its first gated call would have.
    #[test]
    fn a_beacon_without_tab_identity_is_a_no_op_and_an_unknown_tab_is_created() {
        let reg = LatchRegistry::default();
        let out = reg.beacon(None, "WebSearch", ON, BEACON_PROV);
        assert_eq!(out, BeaconOutcome::inert());
        assert!(
            reg.snapshot().is_empty(),
            "an identityless beacon must not create a row"
        );
        // First contact for this tab is the beacon itself.
        let fresh = scope("claude-9", Some("sess-z"));
        assert_eq!(
            reg.beacon(Some(&fresh), "WebFetch", ON, BEACON_PROV)
                .view
                .latch,
            "external"
        );
        let rows = reg.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tab, "claude-9");
    }

    /// A beacon arriving while the tab is LOCAL-latched cannot refuse the fetch
    /// — the harness already ran it — so it records the contamination and
    /// leaves the latch where it is. That is the honest reading: this
    /// conversation has now seen external content, and its proxied external
    /// side stays closed.
    #[test]
    fn a_beacon_under_a_local_latch_contaminates_without_flipping() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert_eq!(
            out.view.latch, "local",
            "sticky: a beacon never flips a latch"
        );
        assert!(out.view.contaminated);
        // #45: no transition ⇒ no activity row. The contamination is real, but
        // the latch did not move, and a row per beacon would let a caller flood
        // the feed.
        assert!(!out.engaged);
        // The contamination is what bites: the memory write is quarantined even
        // though the latch says `local`.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined)
        );
    }

    /// Locked decision 15's state machine. `flip_local` applies ONLY from
    /// External (there is nothing to flip from Open, and from Local it would be
    /// a no-op that reads like an action); `unlatch` applies from either
    /// latched state and not from Open.
    #[test]
    fn flip_local_applies_only_from_external_and_unlatch_from_any_latch() {
        // Open: neither move applies.
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_outline",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg
            .apply_override(&s, LatchOverride::FlipLocal)
            .is_err_and(|e| e.contains("EXTERNAL-latched")));
        assert!(reg
            .apply_override(&s, LatchOverride::Unlatch)
            .is_err_and(|e| e.contains("not latched")));

        // Local: flip is refused (it is already there), unlatch works.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(reg.apply_override(&s, LatchOverride::FlipLocal).is_err());
        let out = reg
            .apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch applies from local");
        assert_eq!(out.prior, Latch::Local);
        assert_eq!(out.view.latch, "open");

        // External: the flip is the workflow button.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let out = reg
            .apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip applies from external");
        assert_eq!(out.prior, Latch::External);
        assert_eq!(out.view.latch, "local");
        assert!(out.view.contaminated);
        assert!(!out.view.can_flip_local, "no second flip to offer");
        assert!(out.view.can_unlatch);

        // A tab the proxy has never served has no latch to override at all.
        let reg = LatchRegistry::default();
        assert!(reg
            .apply_override(&s, LatchOverride::Unlatch)
            .is_err_and(|e| e.contains("nothing to override")));
    }

    /// The flip is the decision-15 workflow: research done, now apply it. It
    /// restores the proxied local-capability tools and CLOSES the external side
    /// in the same move — at no instant does the session hold both.
    ///
    /// **#48 (F-34) SPLIT this test rather than loosening it.** It used to assert
    /// `Err(REFUSAL_EXTERNAL_BLOCKED)` for the closed side, which pinned the
    /// defect's *shape* — the string that says *"this task has already used a
    /// local-capability tool"* about a latch no tool call moved. What it is FOR
    /// is containment: after the flip the external side is closed, exactly and
    /// only. That is what stays here, still asserted against an exact constant.
    /// Which sentence each cause gets is
    /// [`the_proxied_external_refusal_names_the_user_flip_only_when_a_user_flipped_it`],
    /// and the old constant is still pinned there for the case where it is true.
    #[test]
    fn flip_local_reopens_local_tools_and_closes_the_external_side() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED)
        );
        reg.apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_USER_LOCAL),
            "the external side is closed — and by the flip, which is what closed it"
        );
    }

    /// **#48 (F-34): the proxied external refusal states the cause the gate
    /// checked — F-23's twin, on the route that ships ON.**
    ///
    /// The defect: after the user clicked "Switch to local", a proxied external
    /// call was refused with `REFUSAL_EXTERNAL_BLOCKED` — *"this task has already
    /// used a local-capability tool"*. False. No tool call latched that tab; the
    /// user's own IPC flip did. Observed live, a tab's model believed the string
    /// and told its user that `graph_snippet` had caused the latch: a confident,
    /// wrong causal story about a security event.
    ///
    /// Both halves are asserted, because the fix is a *split*, not a rename — the
    /// old constant is the TRUE statement for a latch a tool call earned, and a
    /// fix written as "local ⇒ the user did it" fails the first case below.
    ///
    /// It also proves the invariant locked decision 34 inherits from F-23: **the
    /// flag cannot outlive the latch it explains.** Both exits from `Local` are
    /// walked on this route — the rotation reset and the unlatch — and after each
    /// one a latch re-earned by a *tool call* is refused with the old constant
    /// again. A stale `true` would be F-34 with the operands swapped: a tool
    /// call's latch reported to the model as the user's decision.
    #[test]
    fn the_proxied_external_refusal_names_the_user_flip_only_when_a_user_flipped_it() {
        // 1. EARNED by a local-capability tool call. The pre-F-34 sentence is the
        //    true one here and must survive untouched.
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.view_for(&s).latch, "local");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "a tool call really did close this side"
        );

        // 2. The finding's own path: fetch → EXTERNAL → the user's workflow flip.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_USER_LOCAL),
            "no tool call closed this side, and the refusal must not say one did"
        );
        // Containment is byte-identical: only the sentence moved. The local side
        // is open (that is what the flip is FOR) and the write path is unchanged.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        // …and the row a reviewer reads carries the same corrected sentence,
        // rather than the feed and the model being told different stories.
        outbound::test_rows::reset();
        let s2 = scope("claude-2", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s2),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s2, LatchOverride::FlipLocal)
            .expect("flip");
        let _ = outbound::test_rows::drain();
        assert_eq!(
            reg.gate(Some(&s2), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_USER_LOCAL)
        );
        let rows = outbound::test_rows::drain();
        let refusals = outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal);
        assert_eq!(refusals.len(), 1);
        assert_eq!(
            refusals[0].response, REFUSAL_EXTERNAL_USER_LOCAL,
            "the incident row quotes what the model was told, verbatim"
        );

        // 3. The flag cannot outlive its latch — exit A, the rotation reset. The
        //    NEXT conversation's own `graph_snippet` is what closes its external
        //    side, and it must be told so.
        let rotated = scope("claude-1", Some("sess-b"));
        assert_eq!(reg.view_for(&rotated).latch, "open", "the rotation reopened");
        assert!(reg
            .gate(
                Some(&rotated),
                LatchRoute::Proxied,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&rotated),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "F-34 with the operands swapped: this one really was a tool call"
        );

        // 4. Exit B, the unlatch. Both sides open again, so there is nothing to
        //    explain; and a latch re-earned afterwards reports the tool call.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Proxied,
                "graph_snippet",
                ON,
                NO_CONTENT
            ),
            Err(REFUSAL_LOCAL_BLOCKED),
            "the unlatch is not a free pass: the tab re-latched EXTERNAL"
        );

        // 5. The user flip is the ONLY thing that selects the new sentence. A
        //    tab whose contamination the user cleared, and one the user armed for
        //    a session clear, are user actions too — neither leaves the latch
        //    `local` by decision, and neither may borrow the flip's sentence.
        let (reg, s) = contaminated_local_registry();
        for action in [
            // Ordered so each precondition holds: the arm needs the bit set, the
            // clear consumes it.
            LatchOverride::AwaitSessionClear,
            LatchOverride::ClearContamination,
        ] {
            reg.apply_override(&s, action).expect("applies");
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
                Err(REFUSAL_EXTERNAL_BLOCKED),
                "{action:?}: the latch is still the one graph_snippet earned"
            );
        }
    }

    /// **#48 (F-23): a `local` latch carries the reason it is `local`,** because
    /// the two reasons are different statements and the native-web refusal has to
    /// make the one it checked.
    ///
    /// The defect: after the user's flip the OpenCode gate served
    /// `REFUSAL_NATIVE_WEB_BLOCKED` — *"this session has already used a
    /// local-capability tool"* — and a live tab's model believed it and told its
    /// user that `graph_snippet` had latched the session. No such call happened;
    /// a human clicked. The fix records WHY at the one site that knows and
    /// publishes it on the wire the gate reads.
    ///
    /// Every assertion here is about the FACT, not about the message: the message
    /// is a fixed constant selected on this boolean, and which sentence the
    /// generated plugin serves for it is pinned in `tabs::config`.
    #[test]
    fn a_user_flipped_local_latch_records_that_no_tool_call_closed_the_web_side() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));

        // A `local` latch EARNED by a local-capability tool: the pre-F-23
        // sentence is the true one here, so the flag must stay false. This is the
        // assertion that fails if the fix is written as "local ⇒ the user did it".
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, "graph_snippet", ON, NO_CONTENT)
            .is_ok());
        let earned = reg.view_for(&s);
        assert_eq!(earned.latch, "local");
        assert!(!earned.local_by_user_flip, "a tool call latched this tab");

        // The finding's own path: fetch → EXTERNAL → the user's workflow flip.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert!(!reg.view_for(&s).local_by_user_flip, "external, not flipped");
        let out = reg
            .apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        assert_eq!(out.view.latch, "local");
        assert!(
            out.view.local_by_user_flip,
            "the flip is the cause, and it is the app that knows it"
        );

        // It reaches the plugin on the wire it already reads — the whole point,
        // since the refusal is thrown inside the harness's own process.
        let (settings, _id) = oc_settings();
        let reply = latch_state_reply(
            &settings,
            &LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
            reg.view_for(&s),
        );
        assert_eq!(reply["latch"], "local", "{reply}");
        assert_eq!(reply["local_by_user_flip"], true, "{reply}");
        // A tab the proxy never served says `false` rather than nothing: the
        // plugin reads the field defensively, and "absent" must not be able to
        // mean "the user flipped it".
        assert_eq!(
            latch_state_reply(&settings, &LatchScoping::Anonymous, LatchView::default())
                ["local_by_user_flip"],
            false
        );

        // It cannot outlive the latch it explains — in either direction out of
        // `local`. An unlatch reopens both sides…
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        assert!(!reg.view_for(&s).local_by_user_flip, "web is open again");

        // …and a session rotation reopens the latch, after which the NEXT
        // conversation's own file read is what closed its web side. Left set, that
        // tab would be told a user flipped a latch nobody touched.
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        assert!(reg.view_for(&s).local_by_user_flip);
        let rotated = scope("claude-1", Some("sess-b"));
        let after = reg.view_for(&rotated);
        assert_eq!(after.latch, "open", "the rotation reopened the latch");
        assert!(!after.local_by_user_flip, "and the reason went with it");
        assert!(reg
            .gate(
                Some(&rotated),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let relatched = reg.view_for(&rotated);
        assert_eq!(relatched.latch, "local");
        assert!(
            !relatched.local_by_user_flip,
            "F-23 with the operands swapped: this one really was a tool call"
        );
    }

    /// **The core Phase F invariant, as decision 15's 2026-08-10 amendment
    /// leaves it.** Contamination is a property of the CONVERSATION, not of the
    /// latch position: a note written after the *flip* was still composed by a
    /// model that read an attacker's page, so persistence stays quarantined
    /// through it.
    ///
    /// H-2 extends it past the session boundary: this test used to end by
    /// rotating the session and asserting a clean scope ("a tab restart, the one
    /// clean exit the UI names"). It now asserts the opposite, because the
    /// rotation signal comes from a file the model's own Bash can create — see
    /// [`TabLatch::contaminated`]. The latch still reopens; the bit does not.
    ///
    /// **Why the name narrowed** (it was
    /// `contamination_survives_every_override_and_every_session_rotation`): the
    /// user's 2026-08-10 decision moved `unlatch` out of this rule — *"if the
    /// user restores full access then the tab should be cleared, it's the user's
    /// decision."* The flip is a workflow step and keeps the bit; the unlatch is
    /// a verdict and releases it, which is
    /// `a_full_unlatch_clears_contamination_and_records_it` next door. Of the
    /// four actions only `clear_contamination` and `unlatch` clear; the flip
    /// never does, and the arm defers.
    #[test]
    fn contamination_survives_the_flip_and_every_session_rotation() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let out = reg
            .apply_override(&s, LatchOverride::FlipLocal)
            .expect("the flip applies from external");
        assert!(
            out.view.contaminated,
            "the flip is a workflow step, not a verdict"
        );
        assert!(
            out.prior_taint.is_none(),
            "and it releases nothing, so it has no prior taint to record"
        );
        // The latch moved; the quarantine did not.
        assert_ne!(out.view.latch, "external");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined),
            "a post-flip write must still be quarantined"
        );
        assert!(reg.snapshot()[0].view.contaminated);

        // H-2: a new session id reopens the latch — but the write is STILL
        // quarantined, because "the session rotated" is a claim sourced from
        // an attacker-writable transcript directory.
        let after = scope("claude-1", Some("sess-b"));
        assert_eq!(
            reg.gate(
                Some(&after),
                LatchRoute::Native,
                "context_note",
                ON,
                NO_CONTENT
            ),
            Ok(WriteTaint::Quarantined),
            "a rotation must not re-open the persistence channel"
        );
        let rows = reg.snapshot();
        assert!(rows[0].view.contaminated);
        assert_eq!(rows[0].latch(), "open");

        // And the fourth action likewise leaves the bit alone — one assertion,
        // because the full behaviour is `a_restore_arms_the_wait_and_clears_
        // nothing_now` and duplicating it here would give the rule two homes.
        let (armed_reg, armed_scope) = contaminated_registry();
        let armed = armed_reg
            .apply_override(&armed_scope, LatchOverride::AwaitSessionClear)
            .expect("arm");
        assert!(
            armed.view.contaminated,
            "the restore arm defers the clear; it does not perform one"
        );
    }

    /// **Decision 15's 2026-08-10 amendment** (user: *"if the user restores full
    /// access then the tab should be cleared, it's the user's decision."*). One
    /// invariant with several faces, so one test: the state, the payload the user
    /// is buying, the prior state the audit rows quote, the two rows themselves,
    /// and the two cases where nothing is released.
    ///
    /// The trust root is the one that closed H-2 — **authority, not evidence**.
    /// An attacker cannot click this; the click already hands back the strictly
    /// more dangerous capability, so leaving persistent memory quarantined
    /// afterwards overruled a judgement the product had just asked for.
    #[test]
    fn a_full_unlatch_clears_contamination_and_records_it() {
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();

        // 1. The state. `can_clear` goes with the bit — there is nothing left to
        //    clear, so the popover must stop offering it.
        let out = reg
            .apply_override(&s, LatchOverride::Unlatch)
            .expect("a contaminated latched tab can be unlatched");
        assert!(!out.view.contaminated, "the flag went with the access");
        assert!(!out.view.can_clear, "and nothing is left to clear");
        assert!(!out.view.awaiting_session_clear);
        assert_eq!(out.view.latch, "open");
        assert!(!reg.snapshot()[0].view.contaminated);

        // 2. Prior state, captured BEFORE the latch moved. `external`, not
        //    `open`: this is the assertion that goes red if someone moves the
        //    clear after `entry.latch = Latch::Open`, which would make the audit
        //    row quote the state the click produced instead of the one it
        //    replaced.
        let prior = out.prior_taint.as_ref().expect("the clear happened here");
        assert_eq!(prior.latch, "external");
        assert_eq!(prior.session.as_deref(), Some("sess-a"));

        // 3. Two rows, right lanes, right words. Neither is written by the
        //    registry — both are composed here and filed by
        //    `apply_latch_override`, the IPC entry point, from one stated origin.
        let orow = override_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &out);
        assert_eq!(orow.screen, outbound::Screen::LatchOverride);
        assert_eq!(orow.tool, "unlatch");
        let d = &orow.detail;
        assert!(d.contains("FULL access restored"), "{d}");
        assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
        assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
        assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");

        let cleared = unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &s, &out)
            .expect("a release owes the contamination_cleared lane a row");
        assert_eq!(cleared.basis.tool(), "unlatch");
        assert_eq!(
            cleared.session.as_deref(),
            Some("sess-a"),
            "filed under the CONTAMINATED conversation, so it joins the row that opened it"
        );
        assert_eq!(cleared.root, TEST_ROOT, "an empty root defeats the row");
        let cd = &cleared.detail;
        assert!(cd.contains("basis: unlatch"), "{cd}");
        assert!(cd.contains("origin: ipc"), "{cd}");
        assert!(cd.contains("contaminated=true"), "the PRIOR state: {cd}");
        assert!(cd.contains("STAY quarantined"), "decision 10 stated: {cd}");

        // 4. Empty is not absent: the clear releases STATE, never EVIDENCE. The
        //    `contamination` row that set the bit is untouched by the override —
        //    what makes "cleared" distinguishable from "never contaminated" is
        //    that pair of rows, not the live view, which is now identical.
        let rows = outbound::test_rows::drain();
        assert_eq!(
            contamination_rows(&rows).len(),
            1,
            "the row that SET the bit is still in the feed after the release"
        );
        assert!(
            cleared_rows(&rows).is_empty(),
            "the release's own row is the IPC entry point's to write, exactly as the \
             resume's is — see `unlatch_clear_row`"
        );

        // 5. The payload the user is actually buying, and the reason this action
        //    and not the flip: BOTH holds are released, so the next memory write
        //    is stored clean.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean),
            "decision 15's 2026-08-10 amendment: restoring full access releases the \
             flag too — the user's decision"
        );

        // 6. The honest `None`: an unlatch on a tab that was never contaminated
        //    is still legal, releases nothing, and must not write a row claiming
        //    a bit was released.
        let clean = LatchRegistry::default();
        let cs = scope("claude-2", Some("sess-c"));
        assert!(clean
            .gate(
                Some(&cs),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let cout = clean
            .apply_override(&cs, LatchOverride::Unlatch)
            .expect("an uncontaminated latched tab unlatches too");
        assert_eq!(cout.prior, Latch::Local);
        assert!(cout.prior_taint.is_none(), "there was nothing to release");
        assert!(
            unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &cs, &cout).is_none(),
            "a `contamination_cleared` row here would claim a release that never happened"
        );
        let cleanrow = override_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &cout);
        assert!(
            cleanrow.detail.contains("nothing to clear"),
            "{}",
            cleanrow.detail
        );

        // 7. An arm is superseded, not stranded: `clear_contamination` drops it,
        //    because once the bit is gone there is nothing left to wait for.
        outbound::test_rows::reset();
        let (armed, arm_s) = contaminated_registry();
        armed
            .apply_override(&arm_s, LatchOverride::AwaitSessionClear)
            .expect("arm");
        let _ = outbound::test_rows::drain();
        let aout = armed
            .apply_override(&arm_s, LatchOverride::Unlatch)
            .expect("unlatch supersedes the arm");
        assert!(!aout.view.contaminated);
        assert!(
            !aout.view.awaiting_session_clear,
            "an arm outliving its bit is a trap waiting for the next rotation"
        );
        assert!(
            aout.prior_taint.as_ref().is_some_and(|p| p.armed),
            "and the row records that the tab had been armed"
        );
        assert!(
            cleared_rows(&outbound::test_rows::drain()).is_empty(),
            "still exactly one release row, and it is the builder's"
        );
        assert!(unlatch_clear_row(
            outbound::Origin::Ipc,
            LatchOverride::Unlatch,
            &arm_s,
            &aout
        )
        .is_some());

        // 8. And the collision case the ordering inside `apply_override` decides:
        //    an armed one-shot that fires on the SAME click (the user restored,
        //    ran `/clear`, then clicked). `observe` runs first, so it clears the
        //    bit and writes the rotation's row; the unlatch then finds a latch
        //    the rotation already reopened and is refused — while the clear that
        //    really happened is still recorded. Exactly one release row.
        outbound::test_rows::reset();
        let (raced, rs) = contaminated_registry();
        raced
            .apply_override(&rs, LatchOverride::AwaitSessionClear)
            .expect("arm");
        let _ = outbound::test_rows::drain();
        let rotated = scope("claude-1", Some("sess-b"));
        let err = raced
            .apply_override(&rotated, LatchOverride::Unlatch)
            .expect_err("the rotation reopened the latch, so there is nothing to unlatch");
        assert!(err.contains("not latched"), "{err}");
        assert_eq!(
            cleared_rows(&outbound::test_rows::drain()).len(),
            1,
            "a refused action must not swallow a clear that already happened"
        );
        assert!(!raced.snapshot()[0].view.contaminated);

        // 9. And from `Open`, contaminated: the H-2 state (a rotation reopened
        //    the latch and kept the bit). `unlatch` does not apply there and
        //    clears nothing — `clear_contamination` is that state's action, which
        //    is why `can_unlatch` is deliberately not widened.
        let (open_reg, _os) = contaminated_registry();
        let orotated = scope("claude-1", Some("sess-b"));
        let v = open_reg.view_for(&orotated);
        assert_eq!(v.latch, "open");
        assert!(v.contaminated, "unarmed: the bit is sticky (H-2)");
        let oerr = open_reg
            .apply_override(&orotated, LatchOverride::Unlatch)
            .expect_err("nothing to unlatch");
        assert!(oerr.contains("not latched"), "{oerr}");
        assert!(
            open_reg.snapshot()[0].view.contaminated,
            "a refused unlatch releases nothing"
        );
    }

    /// Full unlatch restores both sides — the at-own-risk move — **and, since
    /// decision 15's 2026-08-10 amendment, persistence with them**: the user
    /// restored full access, and the flag goes with it. Both facts matter: the
    /// button must actually work, and it must release exactly what the
    /// confirmation says it releases.
    ///
    /// This test asserted the inverse until 2026-08-11 (it was
    /// `full_unlatch_restores_both_sides_but_not_persistence`, ending in
    /// *"unlatching must not un-contaminate the conversation"*). The flip keeps
    /// that rule — see
    /// `contamination_survives_the_flip_and_every_session_rotation`.
    ///
    /// **It also needed splitting into two legs, and that is a finding of its
    /// own.** The old single-registry version probed the external side first
    /// (`ddg__search`) and *then* asserted the write was quarantined — but that
    /// probe is an admitted EXTERNAL call: it re-latches the tab EXTERNAL and
    /// re-contaminates the conversation. The quarantine it observed was the NEW
    /// latch's, on `Latch::proxy_gate`'s own authority, so the assertion never
    /// depended on the unlatch's treatment of the flag at all. Two registries,
    /// because one call cannot both exercise the web side and leave the tab in
    /// the state the other half is about.
    #[test]
    fn full_unlatch_restores_both_sides_including_persistence() {
        // Leg 1: the web side answers again — the button does what it says.
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        // …and that call has re-latched EXTERNAL and re-contaminated the tab,
        // which is correct: a new page really was read.
        assert!(reg.snapshot()[0].view.contaminated);

        // Leg 2: and so does persistence, which is what the amendment added.
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean),
            "decision 15's 2026-08-10 amendment: restoring full access also releases \
             the flag — the user's decision"
        );
        // The local-capability side answers too (that write re-latched Local).
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
    }

    /// The wire vocabulary. An unrecognized action is an ERROR, never resolved
    /// to a default — the moves differ in exactly how much capability they hand
    /// back, so a typo must not pick one.
    ///
    /// The literal list below is the *assertion*, not the input (the same shape
    /// `screen_labels_are_the_distinct_wire_values` takes): a fifth action fails
    /// here until someone gives it a wire value and names it, because the
    /// frontend's `LatchAction` union is a hand-kept mirror of exactly this set.
    #[test]
    fn latch_override_parses_exactly_the_declared_actions() {
        const ACTIONS: [(LatchOverride, &str); 4] = [
            (LatchOverride::FlipLocal, "flip_local"),
            (LatchOverride::Unlatch, "unlatch"),
            (LatchOverride::ClearContamination, "clear_contamination"),
            (LatchOverride::AwaitSessionClear, "await_session_clear"),
        ];
        for (action, wire) in ACTIONS {
            assert_eq!(action.as_str(), wire);
            assert_eq!(LatchOverride::parse(wire), Ok(action), "{wire}");
            // Trimmed, exactly as `unlatch` always was.
            assert_eq!(LatchOverride::parse(&format!(" {wire} ")), Ok(action));
        }
        for junk in [
            "",
            "unlatch_all",
            "flip",
            "FLIP_LOCAL",
            "open",
            // Near-misses of the two new ones. An action that CLEARS containment
            // is the last place a lenient parse belongs.
            "clear",
            "clear_contamination_now",
            "await_session",
            "session_clear_observed",
        ] {
            assert!(LatchOverride::parse(junk).is_err(), "{junk}");
        }
    }

    // ── #45 — the registry's bound, and the audit row's provenance ──────────

    /// Settings carrying `ids` as AI tabs, plus one reserved Shell tab (which
    /// hosts no harness and must therefore never be a valid latch scope).
    fn settings_with_tabs(ids: &[&str]) -> crate::settings::Settings {
        settings_with_consumer_tabs(&ids.iter().map(|id| ("claude", *id)).collect::<Vec<_>>())
    }

    /// V33 C5: settings carrying `(consumer, id)` AI tabs — `"claude"` builds a
    /// Claude tab, anything else an OpenCode one — plus the same reserved Shell
    /// tab. The consumer of a tab is its COMMAND (`tabs::tab_consumer`), so
    /// these are built from the real defaults rather than by stamping a field.
    fn settings_with_consumer_tabs(tabs_in: &[(&str, &str)]) -> crate::settings::Settings {
        use crate::settings::{default_ai_tab, default_graph_monitor_tab, AiTabId, TabConfig};
        let mut tabs = vec![default_graph_monitor_tab()];
        for (consumer, id) in tabs_in {
            let mut t = default_ai_tab(if *consumer == "claude" {
                AiTabId::Claude
            } else {
                AiTabId::OpenCode
            });
            if let TabConfig::AiTool(c) = &mut t {
                c.id = (*id).to_string();
            }
            tabs.push(t);
        }
        crate::settings::Settings {
            tabs,
            ..Default::default()
        }
    }

    /// V33: `/context/retrieve` accepts an optional `tab`, and only a
    /// **configured** one becomes the checkpoint's identity.
    ///
    /// Covers the three cases that must stay apart at this boundary: a real tab
    /// (recorded), a forged/stale one (dropped — never written as a fabricated
    /// attribution), and a body from a shim old enough not to send the field at
    /// all (parses fine, records no tab, exactly the pre-V33 row).
    ///
    /// **What it would still pass with if the change regressed:** a handler
    /// that recorded `body.tab` verbatim would fail the forged case; a handler
    /// that dropped the tab entirely would fail the configured case; a
    /// `#[serde(default)]` removed from `tab` would fail the old-shim case with
    /// a parse error, which is what turns "no identity" into "no context
    /// injection for that user at all".
    #[test]
    fn context_retrieve_records_only_a_configured_tab_as_checkpoint_identity() {
        let s = settings_with_tabs(&["claude", "claude-2"]);
        let parse = |json: &str| -> ContextRetrieveBody {
            serde_json::from_str(json).expect("body parses")
        };

        // A real tab: recorded, alongside the session and agent.
        let body = parse(
            r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude","tab":"claude-2"}"#,
        );
        let origin = checkpoint_origin(&s, &body);
        assert_eq!(origin.tab.as_deref(), Some("claude-2"));
        assert_eq!(origin.session.as_deref(), Some("sess-1"));
        assert_eq!(origin.agent.as_deref(), Some("claude"));

        // A forged / stale id: dropped, not recorded as fact. The session still
        // is — it widens nothing and still improves the join materially.
        let body = parse(
            r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude","tab":"claude-99"}"#,
        );
        let origin = checkpoint_origin(&s, &body);
        assert_eq!(origin.tab, None);
        assert_eq!(origin.session.as_deref(), Some("sess-1"));

        // A pre-V33 shim: no `tab` field at all. Must parse, and must record
        // the pre-V33 shape rather than failing the prompt.
        let body = parse(r#"{"cwd":"P:/p","prompt":"hi","session_id":"sess-1","agent":"claude"}"#);
        let origin = checkpoint_origin(&s, &body);
        assert_eq!(origin.tab, None);
        assert_eq!(origin.agent.as_deref(), Some("claude"));

        // Blank spellings of "no identity" never read as one.
        let body = parse(r#"{"prompt":"hi","session_id":"  ","agent":"","tab":"   "}"#);
        let origin = checkpoint_origin(&s, &body);
        assert_eq!(origin, crate::workbench::shadow::Origin::default());
    }

    /// **V33 Phase F: `/workbench/tool_checkpoint` narrows the tab exactly as
    /// the prompt tap does, and reads the tool name in the CALLER's
    /// vocabulary.**
    ///
    /// The vocabulary half is the subtle one. `toolclass::TABLE` is cImp's own
    /// namespace, where `edit` is an unknown name and `mutates_fs` therefore
    /// answers `false`; `OPENCODE_NATIVE_TABLE` is the harness's, where `Edit`
    /// is unknown for the mirror reason. Crossing them would not fail loudly —
    /// it would silently disable one harness's entire seam while every test that
    /// only exercised the other stayed green. Both directions are asserted.
    #[test]
    fn the_tool_checkpoint_route_narrows_the_tab_and_reads_the_right_vocabulary() {
        let s = settings_with_tabs(&["claude", "claude-2"]);

        // Identity: same funnel, same answers as the prompt tap.
        let origin = checkpoint_identity(&s, Some("claude"), Some("sess-1"), Some("claude-2"));
        assert_eq!(origin.tab.as_deref(), Some("claude-2"));
        assert_eq!(origin.session.as_deref(), Some("sess-1"));
        assert_eq!(
            checkpoint_identity(&s, Some("claude"), Some("sess-1"), Some("claude-99")).tab,
            None,
            "a forged or stale tab id must degrade to `cannot attribute`"
        );
        // The route composes `source` itself and never takes one from the wire,
        // so `Origin::with_source` is the only way a checkpoint gets one.
        assert_eq!(origin.source, None);

        // Vocabulary: Claude's capitalized natives.
        for tool in ["Edit", "Write", "MultiEdit", "Bash"] {
            assert!(tool_checkpoint_is_mutating("claude", tool), "{tool}");
        }
        for tool in ["Read", "Grep", "WebFetch", "edit"] {
            assert!(!tool_checkpoint_is_mutating("claude", tool), "{tool}");
        }
        // …and OpenCode's lowercase ids, which are a DIFFERENT table.
        for tool in ["edit", "write", "patch", "apply_patch", "bash"] {
            assert!(tool_checkpoint_is_mutating("opencode", tool), "{tool}");
        }
        for tool in ["read", "grep", "glob", "webfetch", "task", "Edit"] {
            assert!(!tool_checkpoint_is_mutating("opencode", tool), "{tool}");
        }
        // An unrecognised harness normalises to Claude (`hook_agent`'s own
        // default), so it reads Claude's table rather than nothing at all.
        assert!(tool_checkpoint_is_mutating(hook_agent(None), "Bash"));
        assert!(tool_checkpoint_is_mutating(hook_agent(Some("nonsense")), "Bash"));
    }

    /// **The registry's bound, made real.** `latches()`'s doc claimed the map
    /// was "bounded by construction — tab ids are config-derived"; they are
    /// request-derived, and the claim was asserted only in that comment. The
    /// key space is now the user's configured AI tabs, so the map cannot exceed
    /// one entry per tab per agent no matter what a caller POSTs — which
    /// matters because every entry is serialized into every `/status` response
    /// and every 4 s `latch_status` poll, with no TTL, cap or eviction.
    /// **#48 rewrote this test too.** It named a registry bound and exercised
    /// [`is_configured_tab`] directly — a predicate *beside* the enforcement
    /// point, not through it. Deleting the `is_configured_tab` call from
    /// `latch_scope` left it green, so the one thing the issue actually changed
    /// was untested. It now asserts through [`tab_identity`], which is the
    /// decision `latch_scope` delegates to (its remaining work is the session
    /// lookup, which needs an `AppHandle` this crate cannot mock), and then
    /// through the registry itself.
    #[test]
    fn only_configured_ai_tab_ids_can_ever_key_a_latch() {
        let s = settings_with_tabs(&["claude", "claude-2"]);
        assert_eq!(
            tab_identity(&s, "claude", Some("claude")),
            TabIdentity::Configured("claude")
        );
        assert_eq!(
            tab_identity(&s, "claude", Some(" claude-2 ")),
            TabIdentity::Configured("claude-2"),
            "surrounding whitespace is trimmed, not treated as a different tab"
        );

        for forged in ["claude-1", "Claude", "../claude", "graph-monitor"] {
            assert_eq!(
                tab_identity(&s, "claude", Some(forged)),
                TabIdentity::Unknown(forged),
                "{forged:?} is not a configured AI tab and must not key a latch"
            );
        }
        // The two identity-less shapes are distinct (#48): "no tab id" is not
        // "an id I do not recognize", and `handle_latch_state` reads them apart.
        for anon in [None, Some(""), Some("   ")] {
            assert_eq!(
                tab_identity(&s, "claude", anon),
                TabIdentity::Anonymous,
                "{anon:?}"
            );
        }

        // The bound stated as a bound: whatever a caller sends, the set of ids
        // that get through is a subset of the configured AI tabs.
        let attempts = [
            "claude",
            "claude-2",
            "claude-1",
            "claude-3",
            "tab-9999",
            "graph-monitor",
        ];
        let admitted: Vec<&str> = attempts
            .iter()
            .copied()
            .filter(|t| matches!(tab_identity(&s, "claude", Some(t)), TabIdentity::Configured(_)))
            .collect();
        assert_eq!(admitted, ["claude", "claude-2"]);

        // And the bound where it is actually load-bearing: the registry. A
        // forged id resolves to no scope, and the two methods that insert are
        // the only ones that ever receive one — so `/status` and the 4 s
        // `latch_status` poll cannot be grown by a caller inventing ids.
        let reg = LatchRegistry::default();
        for forged in attempts.iter().copied().filter(|t| {
            !matches!(
                tab_identity(&s, "claude", Some(t)),
                TabIdentity::Configured(_)
            )
        }) {
            let scope = match tab_identity(&s, "claude", Some(forged)) {
                TabIdentity::Configured(t) => Some(LatchScope {
                    agent: "claude",
                    tab: t.to_string(),
                    session: None,
                    root: TEST_ROOT.to_string(),
                }),
                _ => None,
            };
            assert!(reg
                .gate(
                    scope.as_ref(),
                    LatchRoute::Proxied,
                    "ddg__search",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            let _ = reg.beacon(scope.as_ref(), "WebFetch", ON, BEACON_PROV);
        }
        assert!(
            reg.snapshot().is_empty(),
            "forged tab ids keyed {} registry entries: {:?}",
            reg.snapshot().len(),
            reg.snapshot()
                .iter()
                .map(|r| r.tab.clone())
                .collect::<Vec<_>>()
        );
    }

    /// The availability floor, stated as a test so it is a decision rather than
    /// an accident: with no AI tab in the snapshot the predicate accepts
    /// everything, because `live_settings` falls back to `Settings::default()`
    /// (empty `tabs`) before managed state is up, and a request in that window
    /// must not be rejected on the strength of a list we could not read.
    ///
    /// **V33 C5 keeps its trigger on the WHOLE list.** The plan's wording was
    /// "narrow the floor to the asserted consumer"; doing that literally would
    /// have *widened* it — on the ordinary install that runs only Claude tabs,
    /// "opencode has zero tabs" would be true forever and every forged id
    /// asserting `consumer: opencode` would get a scope, i.e. the unbounded key
    /// space #45 closed. The condition the floor encodes is "settings are
    /// unreadable", which is global, so only the positive test is
    /// consumer-scoped. The last assertion is the one that would fail if a
    /// future edit moved the floor into `ai_tab_ids_for`.
    #[test]
    fn an_unreadable_tab_list_accepts_rather_than_rejects() {
        let empty = crate::settings::Settings::default();
        assert!(empty.tabs.is_empty(), "the fallback snapshot has no tabs");
        assert!(is_configured_tab(&empty, "claude", "claude-1"));
        assert!(is_configured_tab(&empty, "opencode", "anything"));
        // A snapshot with only reserved Shell tabs is the same case: no AI tab
        // means no list to validate against.
        assert!(is_configured_tab(&settings_with_tabs(&[]), "claude", "anything"));

        // …and a snapshot that HAS tabs is a readable list, for every consumer —
        // including the ones that own none of them.
        let claude_only = settings_with_tabs(&["claude"]);
        assert!(
            !is_configured_tab(&claude_only, "opencode", "claude"),
            "a per-consumer floor would hand every forged id a scope here"
        );
        assert!(
            !is_configured_tab(&claude_only, "opencode", "invented"),
            "a per-consumer floor would hand every forged id a scope here"
        );
    }

    /// **V33 C5 (finding F-4): the `(consumer, tab)` pair is verified.**
    ///
    /// The registry key is the pair ([`LatchScope::key`]) and `agent` is
    /// caller-asserted on every route that has one, but until V33
    /// [`is_configured_tab`] asked only "is this *some* configured AI tab id".
    /// A caller could therefore key a latch under `("claude", <an OpenCode
    /// tab's id>)`, and the pair was checked on no route in the system.
    ///
    /// The review rated the cross-keyed case harmless on `/audit/run` as it
    /// stands — the resulting latch is freshly open and engages a scope nobody
    /// reads — so this pins a restored invariant, not a live exploit.
    ///
    /// **What this would still pass with:** a check that compared the asserted
    /// consumer against a field on the tab config would pass the first two
    /// assertions and fail the third, because there is no such field: the
    /// consumer of a tab is its COMMAND, which is what the launch path splits on
    /// when it decides what to inject (`tabs::tab_consumer`). And a check that
    /// merely rejected mismatches would pass without the `Configured` cases,
    /// which is why both directions are asserted for both consumers.
    #[test]
    fn a_tab_of_one_consumer_cannot_key_a_latch_under_the_other() {
        let s = settings_with_consumer_tabs(&[("claude", "claude"), ("opencode", "opencode")]);

        // Each consumer's own tab resolves.
        assert_eq!(
            tab_identity(&s, "claude", Some("claude")),
            TabIdentity::Configured("claude")
        );
        assert_eq!(
            tab_identity(&s, "opencode", Some("opencode")),
            TabIdentity::Configured("opencode")
        );

        // Cross-keyed, both directions: a real tab id of the OTHER harness is
        // exactly as unrecognized as an invented string, and keys nothing.
        assert_eq!(
            tab_identity(&s, "claude", Some("opencode")),
            TabIdentity::Unknown("opencode"),
            "a caller asserting `claude` must not key a latch under an OpenCode tab"
        );
        assert_eq!(
            tab_identity(&s, "opencode", Some("claude")),
            TabIdentity::Unknown("claude"),
            "…and the reverse"
        );

        // The consumer of a tab is its command, not a stored label — a Claude
        // tab renamed to an OpenCode-looking id is still a Claude tab.
        let renamed = settings_with_consumer_tabs(&[("claude", "opencode-7")]);
        assert_eq!(
            tab_identity(&renamed, "claude", Some("opencode-7")),
            TabIdentity::Configured("opencode-7")
        );
        assert_eq!(
            tab_identity(&renamed, "opencode", Some("opencode-7")),
            TabIdentity::Unknown("opencode-7")
        );

        // And the bound where it is load-bearing: neither cross-keyed attempt
        // reaches the registry, so `/status` cannot be grown by asserting the
        // other consumer's name over a real tab id.
        let reg = LatchRegistry::default();
        for (agent, tab) in [("claude", "opencode"), ("opencode", "claude")] {
            let scope = match tab_identity(&s, agent, Some(tab)) {
                TabIdentity::Configured(t) => Some(LatchScope {
                    agent,
                    tab: t.to_string(),
                    session: None,
                    root: TEST_ROOT.to_string(),
                }),
                _ => None,
            };
            let _ = reg.beacon(scope.as_ref(), "WebFetch", ON, BEACON_PROV);
        }
        assert!(
            reg.snapshot().is_empty(),
            "a cross-keyed beacon created {:?}",
            reg.snapshot()
        );
    }

    // ── V32 C-2 / H-2 — a session rotation must not clear contamination ─────

    /// A tab that has read a page: EXTERNAL-latched, contaminated, session
    /// `real-session`.
    fn contaminated_tab() -> TabLatch {
        let mut t = TabLatch::fresh();
        // A first sighting is not a rotation, so it can never clear anything.
        assert_eq!(t.observe(Some("real-session")), None);
        let scope = LatchScope {
            agent: "claude",
            tab: "claude".to_string(),
            session: Some("real-session".to_string()),
            root: TEST_ROOT.to_string(),
        };
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                Some(&scope),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        // Mirror that admitted EXTERNAL call onto the standalone entry, so the
        // test's subject is built by the same two facts the gate sets.
        t.latch.engage(ToolClass::External);
        t.contaminated = true;
        t
    }

    /// **The seam the whole finding lives on, inverted by H-2.**
    ///
    /// This test used to assert the opposite — that a rotation reaching
    /// [`TabLatch::observe`] CLEARS `contaminated` — on the reading that only a
    /// new conversation has a clean context. C-2 then tried to make the
    /// rotation signal trustworthy, and H-2 showed it cannot be: the signal is
    /// the newest `*.jsonl` under a directory the model's own Bash can write
    /// (decision 3), so every bar over it is a bar over the attacker's own file.
    ///
    /// The rotation still resets everything **permissive** — latch, budget, the
    /// one-row-per-scope report bits — because those are re-earned by the next
    /// real call and a stale one would falsely deny a fresh conversation. It no
    /// longer resets the one bit an attacker would want reset.
    #[test]
    fn a_session_rotation_resets_the_latch_but_never_the_contamination_bit() {
        const ONE_CALL: outbound::BudgetLimits = outbound::BudgetLimits {
            max_calls: 1,
            max_bytes: 0,
        };
        let mut t = contaminated_tab();
        t.latch_flagged = true;
        t.beacon_flagged = true;
        t.budget.charge(4096);
        assert!(t.contaminated && t.latch == Latch::External);
        assert!(t.budget.exhausted(ONE_CALL), "the spend is on the books");

        // Step 4: the return value IS the "did this clear anything" answer, so
        // an unarmed tab must answer `None` — asserted here rather than only
        // through `t.contaminated` below, because a future `observe` that
        // cleared the bit and forgot the row would still leave `contaminated`
        // false and could not be caught by reading the field alone.
        assert_eq!(
            t.observe(Some("aaaa")),
            None,
            "an UNARMED tab clears nothing on a rotation, and reports nothing"
        );
        assert_eq!(t.session.as_deref(), Some("aaaa"), "the id itself rotates");
        assert_eq!(t.latch, Latch::Open, "a rotation reopens the latch");
        assert!(
            !t.latch_flagged,
            "and re-arms the one-row-per-scope reports"
        );
        assert!(!t.beacon_flagged);
        assert!(
            !t.budget.exhausted(ONE_CALL),
            "and refills the fetch budget"
        );
        assert!(
            t.contaminated,
            "H-2: a rotation is a claim about an attacker-writable file, so it may \
             not un-taint the context window — only a user's own click does (step 4)"
        );
        assert!(
            !t.awaiting_session_clear,
            "and nothing about a rotation may ARM the one-shot either"
        );

        // …and the same call with NO id, or the same id, changes nothing. This
        // is the "keep calling until the registry blinks" attack `observe`
        // already defended against; C-2 and H-2 are its harder siblings.
        let mut t = contaminated_tab();
        assert_eq!(t.observe(None), None);
        assert_eq!(t.observe(Some("real-session")), None);
        assert!(t.contaminated && t.latch == Latch::External);
    }

    /// **C-2/H-2, filesystem variant.** A Claude tab's session id is the stem of
    /// the newest `*.jsonl` in its project dir, ranked purely by mtime, and the
    /// tap used to mark a post-attach file live *immediately*
    /// (`live_confirmed = !first_attach`). So `type nul > …/aaaa.jsonl` from
    /// Bash — a zero-byte file — reported session `aaaa` within one 200 ms poll.
    ///
    /// C-2's fix put a growth bar in the tap, and **H-2 walked straight over it
    /// with `echo {} > …/aaaa.jsonl`**: `read_complete_lines` advances the
    /// offset for any newline-terminated bytes, so a trailing `\n` was the whole
    /// bar. The old version of this test asserted `gate.observed(0, 0)` — the
    /// zero-byte PoC's exact shape — which is why one byte of content defeated a
    /// green suite.
    ///
    /// Two independent guards now, and this test states both:
    /// 1. the gate takes a DECODE proof, so bytes alone confirm nothing; and
    /// 2. **even a confirmed rotation cannot clear `contaminated`**, because the
    ///    file the proof is read from is one the attacker writes.
    ///
    /// Asserted **through** `oob::claude::LiveSessionGate` rather than beside
    /// it, so weakening the gate fails this test.
    #[test]
    fn a_forged_rotation_neither_confirms_a_session_nor_clears_contamination() {
        use crate::oob::claude::LiveSessionGate;
        let mut tab = contaminated_tab();
        let mut gate = LiveSessionGate::default();
        // The tap is running on a confirmed session.
        assert!(gate.observed(true));

        // The forged file wins `newest_jsonl` on mtime. The tap rotates onto it
        // and drains. Whatever the attacker wrote — nothing (`type nul`), or
        // bytes that decode to no record of this session (`echo {}`) — the drain
        // reports no evidence, however far the offset moved.
        gate.rotated();
        let live = gate.observed(false);
        assert!(
            !live,
            "a transcript that yields no record naming this session is not live"
        );
        // Ten more polls of the same nothing.
        for _ in 0..10 {
            assert!(!gate.observed(false));
        }
        // So no rotation ever reaches the registry, and the latch keeps the
        // session it was engaged for.
        if live {
            assert_eq!(tab.observe(Some("aaaa")), None);
        }
        assert_eq!(tab.session.as_deref(), Some("real-session"));
        assert_eq!(tab.latch, Latch::External);
        assert!(
            tab.contaminated,
            "contamination survives a transcript file the harness never wrote"
        );

        // H-2's belt-and-braces half: suppose the forger goes one better and
        // writes `{"sessionId":"aaaa"}`, clearing the decode bar. The rotation
        // now DOES reach `observe` — and still cannot un-taint the tab.
        let mut gate = LiveSessionGate::default();
        gate.rotated();
        assert!(gate.observed(true), "a decoded record confirms the session");
        assert_eq!(
            tab.observe(Some("aaaa")),
            None,
            "step 4 must not have widened this: the rotation is admitted, and on an \
             UNARMED tab it still clears nothing"
        );
        assert_eq!(tab.latch, Latch::Open, "the permissive state does reset");
        assert!(
            tab.contaminated,
            "H-2: no filesystem-derived rotation may clear the contamination bit"
        );
    }

    /// The other half of the same rule: a **real** new session — a file the
    /// harness is actually writing into — still rotates the LATCH's scope. The
    /// fix must not buy containment by freezing every tab's latch at its first
    /// session. (What it deliberately does NOT rotate is `contaminated`; that is
    /// the test above.)
    #[test]
    fn a_rotation_with_decoded_evidence_does_reopen_the_latch() {
        use crate::oob::claude::LiveSessionGate;
        let mut tab = contaminated_tab();
        let mut gate = LiveSessionGate::default();
        assert!(gate.observed(true));

        gate.rotated();
        // First poll after the rotation: the harness has created the file but
        // the first line has not landed yet. Still not proof.
        assert!(!gate.observed(false));
        // A line lands that carries no `sessionId` at all (a real shape —
        // `{"type":"file-history-snapshot",…}`). Not evidence either: it neither
        // confirms nor vetoes.
        assert!(!gate.observed(false));
        // Next poll: a decoded record naming this session.
        let live = gate.observed(true);
        assert!(live, "a transcript writing THIS session's records is live");
        // Confirmation is sticky until the next rotation — a quiet turn must
        // not un-confirm a session the tap already proved.
        assert!(gate.observed(false));

        assert_eq!(tab.observe(Some("new-session")), None);
        assert_eq!(tab.latch, Latch::Open);
        assert_eq!(tab.session.as_deref(), Some("new-session"));
        assert!(
            tab.contaminated,
            "a GENUINE rotation into an unarmed tab clears no more than a forged one"
        );
    }

    /// **C-2, token variant.** `/memory/event`'s three `mark_live_session`
    /// calls key the live-session registry on body-supplied strings, with
    /// `agent` defaulting to `"opencode"` and no validation — the #45 check is
    /// on the read side only. One map, two key spaces: the Claude tap keys by
    /// TAB id, OpenCode's loopback path keys by SESSION id. A POST naming a
    /// configured tab id therefore repointed that tab's session and flapped the
    /// latch clear in a loop — and the real tap re-stamping the true id within
    /// 200 ms produced a *second* rotation, so the race helped the attacker.
    ///
    /// Asserted **through** [`mark_live_session_from_event`] — the function the
    /// handler's three sites call — by observing whether the registry write
    /// happens, rather than by calling the predicate beside it. Deleting the
    /// check from that function fails this test.
    #[test]
    fn a_memory_event_cannot_key_the_registry_with_a_tab_id() {
        let s = settings_with_tabs(&["claude", "opencode-2"]);
        // Drive the real function; record what it would have written.
        let written = |settings: &crate::settings::Settings, key: &str| {
            let mut out: Option<String> = None;
            mark_live_session_from_event(|k| out = Some(k.to_string()), settings, "opencode", key);
            out
        };
        for forged in ["claude", "opencode-2"] {
            assert_eq!(
                written(&s, forged),
                None,
                "{forged:?} names a tab, so /memory/event must not key the registry with it"
            );
        }
        // Every legitimate key still gets through: OpenCode session ids are
        // UUIDs, and near-misses of a tab id are not tab ids.
        for real in [
            "ses_01JQ8Z2W6R3K4M5N6P7Q8R9S",
            "b3f1c2d4-5e6f-4708-8910-1112131415",
            "claude-1",
            "Claude",
            " claude",
            "",
        ] {
            assert_eq!(written(&s, real), Some(real.to_string()), "{real:?}");
        }
        // The empty-list escape is deliberately NOT inherited: before settings
        // load, "this string collides with nothing" is the honest answer, and
        // refusing every key in that window would drop real OpenCode telemetry.
        let empty = crate::settings::Settings::default();
        assert_eq!(
            written(&empty, "claude"),
            Some("claude".to_string()),
            "the availability floor belongs to the latch, not to this route"
        );
        assert!(
            is_configured_tab(&empty, "claude", "claude"),
            "…and the latch's own predicate keeps it"
        );
    }

    /// **The override's audit row, which had no coverage at all** — every other
    /// Phase F test calls `apply_override` directly and stops before the row.
    /// The row is the artifact an incident review reads, so its three
    /// load-bearing facts are pinned here: the action, the prior latch (because
    /// "restored full access" from `external` means something very different
    /// from `local`), and that the override did NOT clear contamination.
    #[test]
    fn an_override_row_records_the_action_the_prior_latch_and_the_surviving_taint() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let out = reg
            .apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip applies");
        let row = override_row(outbound::Origin::Ipc, LatchOverride::FlipLocal, &out);
        let detail = &row.detail;
        assert!(detail.contains("USER OVERRIDE (flip_local"), "{detail}");
        assert!(detail.contains("external → local"), "{detail}");
        assert!(detail.contains("contaminated=true"), "{detail}");
        // The row must name the reset that actually works, and step 4 changed
        // what that is. H-2 left "restart cImp" as the only one and the row said
        // so; there are now two user actions, and a row still sending an
        // incident reviewer to a restart would misdirect them.
        assert!(detail.contains("clear_contamination"), "{detail}");
        assert!(detail.contains("await_session_clear"), "{detail}");
        assert!(
            !detail.to_lowercase().contains("restarting cimp"),
            "the restart is no longer the only clean reset: {detail}"
        );
        assert!(!detail.contains("Restarting the tab"), "{detail}");
        assert_eq!(
            row.tool, "flip_local",
            "the action is the row's tool column"
        );
        assert_eq!(
            row.screen,
            outbound::Screen::LatchOverride,
            "a latch move is filed as a latch move"
        );

        // A row that granted capability back must not be painted as a denial.
        assert!(!outbound::Screen::LatchOverride.is_denial());
    }

    /// #45's whole point: the row says WHO asked. An override can now only
    /// arrive over IPC (the HTTP route is gone), and a beacon can only arrive
    /// over HTTP — so the two rows must carry different origins, and the beacon
    /// row must not imply a user acted.
    ///
    /// **#48 rewrote this test, because it could not fail.** It asserted
    /// `detail.contains("origin: ipc")` against a function that spelled
    /// `Origin::Ipc` into its own format string — swapping `Flag.origin` at
    /// both call sites left it green, so the one thing it named (the two rows
    /// are told apart) was untested. The property is that the prose and the
    /// `origin` key have a single source, so it is asserted over EVERY origin
    /// the enum has: whatever a call site states, both halves of the row say
    /// it, and a row whose two halves could disagree fails here.
    #[test]
    fn a_flag_rows_prose_and_its_origin_key_have_one_source() {
        for origin in outbound::Origin::ALL.iter().copied() {
            let reg = LatchRegistry::default();
            let s = scope("claude-1", Some("sess-a"));

            let beacon = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
            assert!(beacon.engaged);
            let brow = beacon_row(origin, "WebFetch", &beacon);
            assert_eq!(brow.origin, origin);
            assert!(
                brow.detail
                    .contains(&format!("origin: {}", origin.as_str())),
                "{:?}: {}",
                origin,
                brow.detail
            );
            // Independent of the origin: a beacon row never implies a human.
            assert!(
                brow.detail.contains("NOT evidence of a user action"),
                "{}",
                brow.detail
            );

            let out = reg
                .apply_override(&s, LatchOverride::Unlatch)
                .expect("unlatch applies");
            let orow = override_row(origin, LatchOverride::Unlatch, &out);
            assert_eq!(orow.origin, origin);
            assert!(
                orow.detail
                    .contains(&format!("origin: {}", origin.as_str())),
                "{:?}: {}",
                origin,
                orow.detail
            );

            // And the machine-readable half agrees with the prose, because it
            // is the same field: this is the assertion that fails if a future
            // call site ever sets `Flag.origin` from anything but `row.origin`.
            for row in [&brow, &orow] {
                let request = outbound::flag_request(&outbound::Flag {
                    screen: outbound::Screen::LatchBeacon,
                    origin: row.origin,
                    consumer: s.agent,
                    scope: &s.label(),
                    attribution: s.attribution(),
                    session: None,
                    tool: &row.tool,
                    host: None,
                    url: None,
                    resolved_ip: None,
                    canary: false,
                    root: String::new(),
                    detail: &row.detail,
                });
                assert_eq!(request["origin"], origin.as_str());
                assert_eq!(request["scope"], "claude:claude-1");
            }
        }

        // The two live call sites still differ, which is the fact #45 bought:
        // an override can only arrive over IPC (the HTTP route is gone) and a
        // beacon only over HTTP.
        assert_ne!(outbound::Origin::Ipc, outbound::Origin::Http);
    }

    /// #48 (A2-2): a beacon that contaminates a conversation **without** moving
    /// the latch writes a row too.
    ///
    /// #45 keyed the row on `engaged` — the latch transition — while
    /// `LatchRegistry::beacon` set `contaminated` unconditionally. A tab already
    /// latched `Local` (Phase A's other direction: a local-capability call came
    /// first) therefore took the contamination bit and left NO trace: no row, no
    /// `warn!`, no `info!`. From that point every `context_note` is quarantined
    /// and every external result enveloped, and the accepted-residuals entry
    /// #45 wrote called the beacon "bounded, audited … and recoverable".
    #[test]
    fn a_beacon_that_only_contaminates_is_recorded_too() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        // A local-capability call first: the tab latches LOCAL, uncontaminated.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local");
        assert!(!reg.snapshot()[0].view.contaminated);

        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(!out.engaged, "the beacon cannot move a LOCAL latch");
        assert!(out.contaminated_now, "but it did contaminate the session");
        assert!(out.report, "and that is a reportable transition");
        assert_eq!(out.view.latch, "local", "decision 15: the latch is unmoved");
        assert!(out.view.contaminated);

        // The row's prose must not claim the latch moved.
        let row = beacon_row(outbound::Origin::Http, "WebFetch", &out);
        assert!(row.detail.contains("CONTAMINATED"), "{}", row.detail);
        assert!(
            !row.detail.contains("now EXTERNAL-latched"),
            "the row must not assert an engagement that did not happen: {}",
            row.detail
        );

        // Still one row per tab-session: a caller in a loop produces no more.
        for _ in 0..5 {
            let again = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
            assert!(!again.report, "the feed must not be floodable");
            assert!(!again.contaminated_now, "and the bit is set only once");
        }
        // …and it is the SESSION that bounds it: a rotation re-arms the report,
        // because a new conversation's contamination is a new fact.
        let rotated = scope("claude-1", Some("sess-b"));
        let after = reg.beacon(Some(&rotated), "WebFetch", ON, BEACON_PROV);
        assert!(after.report, "a rotated session reports again");
    }

    /// The engagement case keeps its single row, and the two transitions do not
    /// double-report: an engaging beacon contaminates and latches at once.
    #[test]
    fn an_engaging_beacon_reports_exactly_once_per_tab_session() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        let first = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(first.engaged && first.contaminated_now && first.report);
        for _ in 0..5 {
            assert!(!reg.beacon(Some(&s), "WebSearch", ON, BEACON_PROV).report);
        }
    }

    /// `/status`'s Phase F shape: the Phase B keys are unchanged (`latch` stays
    /// a top-level key — the flattened view provides it) and the three new
    /// facts sit beside them, so the badge and the override popover read one
    /// row per tab.
    #[test]
    fn status_snapshot_carries_contamination_and_override_availability() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        assert_eq!(
            serde_json::to_value(reg.snapshot()).unwrap(),
            serde_json::json!([{
                "consumer": "claude",
                "tab": "claude-1",
                "session": "sess-a",
                "latch": "external",
                "contaminated": true,
                "can_flip_local": true,
                "can_unlatch": true,
                // Step 4: both contamination moves are on offer, and nothing is
                // waiting. Asserted as an exact object rather than by key, so a
                // field added to the wire without a decision fails here.
                "can_clear": true,
                "awaiting_session_clear": false,
                // #48 (F-23): why the latch is where it is, for the one position
                // with two causes. `external` is not that position.
                "local_by_user_flip": false,
            }])
        );
        // After the flip: still contaminated, no further flip on offer.
        reg.apply_override(&s, LatchOverride::FlipLocal)
            .expect("flip");
        assert_eq!(
            serde_json::to_value(reg.snapshot()).unwrap(),
            serde_json::json!([{
                "consumer": "claude",
                "tab": "claude-1",
                "session": "sess-a",
                "latch": "local",
                "contaminated": true,
                "can_flip_local": false,
                "can_unlatch": true,
                "can_clear": true,
                "awaiting_session_clear": false,
                // #48 (F-23): the flip that just happened is on the row, so the
                // native-web refusal can name the cause it checked instead of
                // blaming a tool call that never ran.
                "local_by_user_flip": true,
            }])
        );
        // After the restore arm: the bit is still set (that is the whole
        // decision) and the tab now says what it is waiting for.
        reg.apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect("arm");
        assert_eq!(
            serde_json::to_value(reg.snapshot()).unwrap(),
            serde_json::json!([{
                "consumer": "claude",
                "tab": "claude-1",
                "session": "sess-a",
                "latch": "local",
                "contaminated": true,
                "can_flip_local": false,
                "can_unlatch": true,
                "can_clear": true,
                "awaiting_session_clear": true,
                // Unchanged by the arm: it waits on the contamination bit and
                // never touches the latch.
                "local_by_user_flip": true,
            }])
        );
    }

    /// A LOCAL-only session is never contaminated: only *external* content can
    /// contaminate, and a clean session must not be dragged into quarantine by
    /// the Phase F bit.
    #[test]
    fn a_purely_local_session_is_never_contaminated() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        for name in ["graph_snippet", "graph_outline", "context_recall"] {
            assert!(
                reg.gate(Some(&s), LatchRoute::Native, name, ON, NO_CONTENT)
                    .is_ok(),
                "{name}"
            );
        }
        assert!(!reg.snapshot()[0].view.contaminated);
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean)
        );
        // A REFUSED external call must not contaminate either — otherwise a
        // hallucinated (or injected) call to the blocked side could quarantine
        // a clean session's memory writes.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        assert!(!reg.snapshot()[0].view.contaminated);
    }

    /// The two Phase F request bodies parse the shapes the beacon shim and the
    /// plugin actually send, and fail open on a missing tab exactly like
    /// `/graph_run` and `/mcp/call` do.
    #[test]
    fn phase_f_bodies_parse_the_shapes_the_reporters_send() {
        let claude: LatchBeaconBody = serde_json::from_slice(
            br#"{"tab":"claude-2","consumer":"claude","tool":"WebFetch","cwd":"P:\\proj","session_id":"s"}"#,
        )
        .expect("claude shim body parses");
        assert_eq!(claude.tab.as_deref(), Some("claude-2"));
        assert_eq!(claude.tool.as_deref(), Some("WebFetch"));

        let bare: LatchBeaconBody =
            serde_json::from_slice(br#"{"consumer":"opencode"}"#).expect("bare body parses");
        assert!(bare.tab.is_none(), "no tab ⇒ fail open, not a 400");

        // There is deliberately no override body type to parse (#45): the
        // override has no wire form, because it has no HTTP route. Its only
        // caller is the `latch_override` IPC command, whose arguments Tauri
        // deserializes into typed parameters.
    }

    /// Fail-open, exactly like the latch: a call with no tab identity has no
    /// scope to charge, so it is never budget-refused.
    #[test]
    fn a_call_without_tab_identity_is_not_budgeted() {
        let reg = LatchRegistry::default();
        for _ in 0..50 {
            assert!(reg.budget_gate(None, TEST_LIMITS, "ddg__search").is_ok());
            reg.charge(None, 100_000);
        }
    }

    // ── #48 finding F-3 — the contamination TRANSITION row ─────────────────
    //
    // Every case below asserts on the rows `record_flag` actually received
    // (`outbound::test_rows`), not on the registry's own return values. That is
    // deliberate: `BeaconOutcome::contaminated_now` and `LatchStatus.contaminated`
    // were already true before this work and F-3 was still open, because
    // "the bit flipped" and "something recorded that the bit flipped" are
    // different facts and only the second one survives the call.

    /// Contamination rows in the order they were written.
    fn contamination_rows(
        rows: &[crate::activity::ActivityRecord],
    ) -> Vec<&crate::activity::ActivityRecord> {
        outbound::test_rows::of_screen(rows, outbound::Screen::Contamination)
    }

    /// One row's request payload, parsed.
    fn payload(row: &crate::activity::ActivityRecord) -> serde_json::Value {
        serde_json::from_str(&row.request).expect("the row's request payload is JSON")
    }

    /// The quarantine-only posture: the switch combination that made the
    /// proxied path contaminate in complete silence.
    const QUARANTINE_ONLY: GatePolicy = GatePolicy {
        latch: false,
        quarantine: true,
    };

    /// The primary path, which before this wrote **nothing at all**: an
    /// admitted proxied EXTERNAL call. One row, carrying when / which tool /
    /// which page / which project / which conversation.
    ///
    /// The "exactly once" half is the other half of the finding: the row must
    /// name the moment the conversation stopped being clean, so a second
    /// EXTERNAL call — which restates a fact this row already carries, and
    /// writes its own ordinary MCP activity row — must not write another.
    #[test]
    fn the_proxied_intake_records_the_contamination_transition_exactly_once() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://evil.example/page"), Some("evil.example")),
            )
            .is_ok());
        // A second EXTERNAL call, in the same conversation, from a different
        // page: the conversation is already contaminated.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                CallProvenance::intake(Some("https://other.example/q"), Some("other.example")),
            )
            .is_ok());

        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(
            hits.len(),
            1,
            "a contaminated conversation must produce exactly one transition row, got {:?}",
            hits.iter().map(|r| &r.entry.tool).collect::<Vec<_>>()
        );
        let row = hits[0];
        // WHEN — the standard stamp, not a field the writer invented.
        assert!(row.entry.ts_ms > 0, "the row has no timestamp");
        // WHICH TOOL — the call that caused the transition, not the later one.
        assert_eq!(row.entry.tool, "ddg__fetch_content");
        // WHICH PROJECT — the field F-3 calls load-bearing. An empty root here
        // makes the row invisible to every per-project surface.
        assert_eq!(row.entry.root, TEST_ROOT);
        assert!(!row.entry.root.is_empty());
        // Nothing was refused: the call was admitted, so the feed must not
        // paint this as a failure.
        assert!(row.entry.ok, "a contamination row is not a denial");
        let req = payload(row);
        assert_eq!(req["screen"], "contamination");
        assert_eq!(req["origin"], "internal");
        assert_eq!(
            req["scope"], "claude:claude-1",
            "the LatchScope::label form"
        );
        // WHICH CONVERSATION — what step 3 will join a checkpoint against.
        assert_eq!(req["session"], "sess-a");
        // FROM WHICH PAGE.
        assert_eq!(req["host"], "evil.example");
        assert_eq!(req["url"], "https://evil.example/page");
        assert_eq!(row.entry.target, "evil.example (claude:claude-1)");
        assert!(
            row.response.contains("CONTAMINATED"),
            "the detail must say what happened: {}",
            row.response
        );
        // The latch the call LEAVES the tab in, not the one it found. A row
        // written before `engage` would say `open` about a tab that is
        // EXTERNAL-latched from this very call — the reader would then look for
        // a second event that never happened.
        assert!(
            row.response.contains("latch=external"),
            "the row quotes the pre-engagement latch: {}",
            row.response
        );
    }

    /// The beacon path records the transition **as well as** its own
    /// `latch_beacon` row. The two are different statements — "this
    /// conversation stopped being clean" and "a harness-native web tool was
    /// detected" — and a build that collapsed them into one would still pass
    /// every count-shaped assertion about "a beacon writes a row".
    #[test]
    fn a_beacon_writes_the_contamination_row_and_its_own_beacon_row() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);

        let rows = outbound::test_rows::drain();
        assert_eq!(contamination_rows(&rows).len(), 1, "no contamination row");
        assert_eq!(
            outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
            1,
            "the beacon row this work must not have displaced"
        );
        let row = contamination_rows(&rows)[0];
        assert_eq!(row.entry.tool, "WebFetch");
        assert_eq!(row.entry.root, TEST_ROOT);
        let req = payload(row);
        // A beacon is a local process POSTing the loopback, never evidence a
        // human acted — the row has to say so (#45).
        assert_eq!(req["origin"], "http");
        assert_eq!(req["scope"], "claude:claude-1");
        assert_eq!(req["session"], "sess-a");
        // Nothing was fetched *through* cImp, so there is no page to name —
        // absent rather than invented.
        assert_eq!(req["host"], serde_json::Value::Null);

        // And a caller in a loop writes neither row again.
        for _ in 0..5 {
            let again = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
            report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &again);
        }
        assert!(
            outbound::test_rows::drain().is_empty(),
            "the transition is over; a loop must not be able to flood the feed"
        );
    }

    /// **The two silent cases F-3 is about.** Both contaminate without moving
    /// any latch, so a fix keyed on the latch transition — or a test that only
    /// exercised the happy path — leaves exactly the bug being fixed.
    #[test]
    fn contamination_is_recorded_even_when_no_latch_moves() {
        // (a) A tab already latched LOCAL. The beacon cannot flip it (the fetch
        //     already happened), so nothing about the latch changes — while
        //     every `context_note` from here on is quarantined.
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "local");
        let _ = outbound::test_rows::drain();

        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(!out.engaged, "a beacon never flips a LOCAL latch");
        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(hits.len(), 1, "the LOCAL-latched case recorded nothing");
        assert_eq!(hits[0].entry.root, TEST_ROOT);
        assert_eq!(payload(hits[0])["scope"], "claude:claude-1");

        // (b) The taint latch feature OFF, the memory quarantine ON. The
        //     contamination bit is still tracked (it is the quarantine's input),
        //     the latch never engages, and this is the posture under which the
        //     proxied path was silent even for a brand-new conversation.
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let t = scope("claude-2", Some("sess-b"));
        assert!(reg
            .gate(
                Some(&t),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                QUARANTINE_ONLY,
                CallProvenance::intake(Some("https://p.example/x"), Some("p.example")),
            )
            .is_ok());
        assert_eq!(
            reg.snapshot()[0].latch(),
            "open",
            "the latch feature is off, so nothing engaged"
        );
        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(hits.len(), 1, "the latch-off case recorded nothing");
        assert_eq!(hits[0].entry.root, TEST_ROOT);
        assert_eq!(payload(hits[0])["host"], "p.example");
        assert_eq!(payload(hits[0])["session"], "sess-b");
        assert!(
            hits[0].response.contains("latch=open"),
            "with the latch feature off the row must not claim a latch: {}",
            hits[0].response
        );
        // The quarantine that follows is the fact the row explains.
        assert_eq!(
            reg.gate(
                Some(&t),
                LatchRoute::Native,
                "context_note",
                QUARANTINE_ONLY,
                NO_CONTENT
            ),
            Ok(WriteTaint::Quarantined)
        );
    }

    /// The row follows the BIT, so everything that does not set the bit writes
    /// nothing: a purely local conversation, a REFUSED external call (which
    /// must never contaminate — that is what keeps a hallucinated call to the
    /// blocked side from quarantining a clean session), a native route's
    /// EXTERNAL-classified name (a typo, not a page), and an inert policy.
    #[test]
    fn nothing_that_leaves_the_conversation_clean_writes_a_contamination_row() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        for name in ["graph_snippet", "graph_outline", "context_recall"] {
            assert!(reg
                .gate(Some(&s), LatchRoute::Native, name, ON, NO_CONTENT)
                .is_ok());
        }
        // EXTERNAL on a NATIVE route: a misspelled native tool, not content.
        assert!(reg
            .gate(Some(&s), LatchRoute::Native, "ddg__search", ON, NO_CONTENT)
            .is_ok());
        // The tab is LOCAL-latched now, so a proxied external call is refused.
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
            ),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        // Both controls off: a disabled control leaves no trace at all.
        const OFF: GatePolicy = GatePolicy {
            latch: false,
            quarantine: false,
        };
        let inert = scope("claude-3", Some("sess-c"));
        assert!(reg
            .gate(
                Some(&inert),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                OFF,
                CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
            )
            .is_ok());
        assert!(
            !reg.beacon(Some(&inert), "WebFetch", OFF, BEACON_PROV)
                .report
        );

        let rows = outbound::test_rows::drain();
        assert!(
            contamination_rows(&rows).is_empty(),
            "a clean conversation was reported as contaminated: {:?}",
            contamination_rows(&rows)
                .iter()
                .map(|r| &r.entry.tool)
                .collect::<Vec<_>>()
        );
        assert!(!reg.snapshot()[0].view.contaminated);
    }

    /// A tab with no identity keys nothing and reports nothing — the fail-open
    /// reading every gate here takes. Stated as a test because the row's whole
    /// value is per-tab attribution, and a row scoped to "(no tab identity)"
    /// would be a row no per-project surface could use.
    #[test]
    fn an_identityless_call_records_no_contamination() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        assert!(reg
            .gate(
                None,
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://evil.example/"), Some("evil.example")),
            )
            .is_ok());
        let _ = reg.beacon(None, "WebFetch", ON, BEACON_PROV);
        let rows = outbound::test_rows::drain();
        assert!(contamination_rows(&rows).is_empty());
    }

    /// **One transition per TAB, not per conversation** — and the row's
    /// `session` therefore names the conversation contamination *started* in.
    ///
    /// This follows H-2 rather than the beacon's own reporting rule, and the
    /// difference is deliberate on both sides. `observe` re-arms
    /// `beacon_flagged` on a proved session rotation (a new conversation may
    /// report a native web tool again) but does **not** clear `contaminated`,
    /// because the rotation signal is a file the model's own shell can write.
    /// So a `/clear` in a contaminated tab keeps the taint, keeps quarantining
    /// its memory writes — and writes no second row, because nothing
    /// transitioned.
    ///
    /// Pinned as a test because a consumer that joins these rows to
    /// conversation-scoped state has to know it: the anchor is the tab's first
    /// contamination, not "the contamination of the session you are looking
    /// at". If the contamination bit ever regains a clear path, this is the
    /// test that has to be revisited with it.
    #[test]
    fn contamination_is_recorded_once_per_tab_across_session_rotations() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let first = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&first),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(None, Some("a.example")),
            )
            .is_ok());
        let rotated = scope("claude-1", Some("sess-b"));
        assert!(reg
            .gate(
                Some(&rotated),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(None, Some("b.example")),
            )
            .is_ok());
        // The rotation did happen — the latch reopened and the budget refilled…
        assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-b"));
        // …and the tab stayed contaminated across it, so there was no second
        // transition to record.
        assert!(reg.snapshot()[0].view.contaminated);
        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(
            hits.len(),
            1,
            "the sticky bit transitioned once, so exactly one row may exist"
        );
        assert_eq!(
            payload(hits[0])["session"],
            "sess-a",
            "the row names the conversation contamination STARTED in"
        );
        assert_eq!(payload(hits[0])["host"], "a.example");
    }

    /// The two paths produce ONE shape of row, because they share
    /// [`note_contamination`]. Asserted over the payload KEYS rather than by
    /// eye: a second writer that drifted (a missing `session`, a different
    /// `scope` spelling) would give the Timeline two shapes to understand.
    #[test]
    fn both_contamination_paths_write_the_same_row_shape() {
        outbound::test_rows::reset();
        let reg = LatchRegistry::default();
        let a = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&a),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://x.example/"), Some("x.example")),
            )
            .is_ok());
        let b = scope("claude-2", Some("sess-b"));
        let out = reg.beacon(Some(&b), "WebFetch", ON, BEACON_PROV);
        report_beacon(Some(&b), outbound::Origin::Http, "WebFetch", &out);

        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(hits.len(), 2);
        let keys = |r: &crate::activity::ActivityRecord| {
            let mut k: Vec<String> = payload(r)
                .as_object()
                .expect("object payload")
                .keys()
                .cloned()
                .collect();
            k.sort();
            k
        };
        assert_eq!(keys(hits[0]), keys(hits[1]));
        for row in &hits {
            assert_eq!(row.entry.source, "contamination");
            assert_eq!(row.entry.kind, "injection_flag");
            assert!(!row.entry.root.is_empty(), "an empty root defeats the row");
            assert!(row.entry.ok);
        }
    }

    // ── Step 4 — the two user-driven contamination clears ──────────────────
    //
    // The governing risk in this area, three findings running: the code is
    // right against the proof-of-concept and wrong against the invariant, and
    // the test pins the PoC's shape. So the cases below are written against the
    // *observable consequence* wherever one exists — a re-contamination row
    // rather than a boolean, a `WriteTaint` rather than a bit — and the two
    // that guard H-2 assert what must NOT happen on a tab nobody armed.

    /// A contaminated, EXTERNAL-latched tab in session `sess-a`, with a page
    /// already fetched (so its budget carries real spend) and both
    /// one-row-per-scope report bits used up.
    fn contaminated_registry() -> (LatchRegistry, LatchScope) {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://evil.example/p"), Some("evil.example")),
            )
            .is_ok());
        assert!(reg.snapshot()[0].view.contaminated);
        (reg, s)
    }

    /// A contaminated tab whose latch is **LOCAL** — the #48 beacon case: the
    /// tab used a local tool first, then the harness's own `WebFetch` reported
    /// in, which contaminates the conversation without moving the latch.
    ///
    /// It exists because of a seam that is easy to test past. Under an EXTERNAL
    /// latch a `context_note` is quarantined by the **latch**
    /// (`Latch::proxy_gate`), whatever the contamination bit says — so a test
    /// that cleared the bit on an EXTERNAL-latched tab and asserted the write
    /// was still held would be asserting the latch's behaviour and calling it
    /// the bit's. On a LOCAL-latched tab the bit is the only thing deciding, so
    /// every assertion about what clearing changes is made here.
    fn contaminated_local_registry() -> (LatchRegistry, LatchScope) {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(!out.engaged, "the latch stays LOCAL");
        assert_eq!(out.view.latch, "local");
        assert!(out.view.contaminated);
        (reg, s)
    }

    /// The rows the clear wrote, in order.
    fn cleared_rows(
        rows: &[crate::activity::ActivityRecord],
    ) -> Vec<&crate::activity::ActivityRecord> {
        outbound::test_rows::of_screen(rows, outbound::Screen::ContaminationCleared)
    }

    /// **A: false-positive resume.** The user judged the flagged content
    /// harmless, so the bit goes now — and *nothing else moves*. The latch keeps
    /// its position, the session keeps its id, the budget keeps its spend.
    ///
    /// Asserting those three is the point rather than padding: "clear the
    /// contamination flag" is a one-line change to a boolean, and the tempting
    /// wrong version of it is `*entry = TabLatch::fresh()`, which would pass any
    /// test that only looked at `contaminated`.
    #[test]
    fn a_false_positive_resume_clears_the_bit_and_touches_nothing_else() {
        let (reg, s) = contaminated_registry();
        // Spend the budget down to its limit so a reset would be visible.
        reg.charge(Some(&s), 100_000);
        assert_eq!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
            Err(outbound::REFUSAL_BUDGET)
        );

        let out = reg
            .apply_override(&s, LatchOverride::ClearContamination)
            .expect("a contaminated tab can be resumed");
        assert!(!out.view.contaminated, "the bit is gone");
        assert!(!out.view.can_clear, "and there is nothing left to clear");
        assert!(!out.view.awaiting_session_clear);

        let row = &reg.snapshot()[0];
        assert_eq!(
            row.session.as_deref(),
            Some("sess-a"),
            "the SESSION is untouched — a resume is not a restart"
        );
        assert_eq!(
            row.latch(),
            "external",
            "and so is the latch: it has its own two buttons"
        );
        assert_eq!(
            reg.budget_gate(Some(&s), TEST_LIMITS, "ddg__search"),
            Err(outbound::REFUSAL_BUDGET),
            "and the fetch budget keeps its spend — a click that refilled it \
             would make the budget advisory"
        );
        // The consequence of leaving the latch alone, stated so nobody reads
        // this feature as more than it is: an EXTERNAL latch quarantines memory
        // writes on its OWN authority (`Latch::proxy_gate`), so clearing the bit
        // does not reopen persistence while the tab is still latched. Reopening
        // it is `unlatch`, which is a separate decision with a separate button.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined),
            "the LATCH still holds writes; clearing the bit is not an unlatch"
        );
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean),
            "…and with both released, writes are clean again"
        );
    }

    /// **B: restore.** The user rolled files back. That cannot un-read a page,
    /// so the bit **stays set** — this action only arms the wait.
    ///
    /// The locked decision is the assertion: a build that "helpfully" cleared on
    /// restore is the exact regression this test exists to catch, and it would
    /// pass any test that merely checked the command succeeded.
    #[test]
    fn a_restore_arms_the_wait_and_clears_nothing_now() {
        // LOCAL-latched, so the quarantine assertion below is about the
        // contamination bit rather than about the latch — see
        // `contaminated_local_registry`.
        let (reg, s) = contaminated_local_registry();
        let out = reg
            .apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect("a contaminated tab can be armed");
        assert!(
            out.view.contaminated,
            "restoring FILES cannot remove injected text from a context window"
        );
        assert!(out.view.awaiting_session_clear, "it arms the one-shot");
        // And the quarantine it gates is still in force for this conversation.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined),
            "a note written after the restore is still held for review"
        );

        // Arming twice is answered, not silently repeated: a second click that
        // reported success would imply something new happened.
        let again = reg
            .apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect_err("a second arm is refused");
        assert!(again.contains("already waiting"), "{again}");
        // …and neither refusal nor repetition may clear anything.
        assert!(reg.snapshot()[0].view.contaminated);
    }

    /// **The critical case: the arm is what decides, not the rotation.**
    ///
    /// Same registry, same decode-proven rotation, two tabs — one armed by a
    /// user, one not. The armed tab clears; the unarmed one does not. If step 4
    /// silently reverted H-2, the second half fails.
    ///
    /// The rotation is driven **through** `oob::claude::LiveSessionGate` rather
    /// than beside it, so a build that weakened the decode proof (H-2's own
    /// guard) fails here too rather than quietly clearing on a forged file.
    #[test]
    fn only_an_armed_tab_clears_on_a_proved_rotation() {
        use crate::oob::claude::LiveSessionGate;

        for armed in [true, false] {
            outbound::test_rows::reset();
            let (reg, s) = contaminated_registry();
            if armed {
                reg.apply_override(&s, LatchOverride::AwaitSessionClear)
                    .expect("arm");
            }
            let _ = outbound::test_rows::drain();

            // The tap proves the new transcript really is this tab's session:
            // a decoded record naming it, which is the ONLY thing that lets a
            // new id reach the live-session registry (H-2).
            let mut live = LiveSessionGate::default();
            live.rotated();
            assert!(!live.observed(false), "no evidence yet, no rotation");
            assert!(live.observed(true), "a decoded record IS the proof");

            // …and only now does a rotated scope reach the registry.
            let rotated = scope("claude-1", Some("sess-b"));
            let view = reg.view_for(&rotated);

            assert_eq!(
                view.contaminated, !armed,
                "armed={armed}: the ARM decides, not the rotation"
            );
            assert!(
                !view.awaiting_session_clear,
                "armed={armed}: a one-shot fires once"
            );
            let rows = outbound::test_rows::drain();
            assert_eq!(
                cleared_rows(&rows).len(),
                usize::from(armed),
                "armed={armed}: a clear writes exactly one row, a non-clear none"
            );

            // The consequence, not the boolean: whether the next memory write is
            // held for review.
            assert_eq!(
                reg.gate(
                    Some(&rotated),
                    LatchRoute::Native,
                    "context_note",
                    ON,
                    NO_CONTENT
                ),
                if armed {
                    Ok(WriteTaint::Clean)
                } else {
                    Ok(WriteTaint::Quarantined)
                },
                "armed={armed}"
            );
        }
    }

    /// **A forged rotation on an unarmed tab still clears nothing** — H-2's own
    /// case, re-run against step 4's code rather than against the code H-2 left.
    ///
    /// Two forgeries, because they fail at two different bars:
    ///
    /// 1. `type nul` / `echo {}` — the transcript yields no record naming the
    ///    session, so `LiveSessionGate` never confirms and no new id ever
    ///    reaches the registry at all.
    /// 2. `echo '{"sessionId":"…"}'` — the decode bar is cleared (decision 3
    ///    puts the model's Bash outside every cImp latch, so it always can be),
    ///    the rotation DOES reach `observe`… and the unarmed tab is still
    ///    contaminated afterwards.
    ///
    /// The deliberate counter-case is in the test above: on an **armed** tab a
    /// forged rotation does clear, and that is the design. The arm is the
    /// authority — an attacker cannot click restore — so a forgery only helps in
    /// the case where the user has already decided the bit should go, and its
    /// worst effect is lifting it slightly earlier than their own `/clear`.
    #[test]
    fn a_forged_rotation_cannot_clear_an_unarmed_tab() {
        use crate::oob::claude::LiveSessionGate;
        let (reg, _s) = contaminated_registry();

        // Forgery 1: bytes, but no record naming this session.
        let mut live = LiveSessionGate::default();
        live.rotated();
        for _ in 0..10 {
            assert!(
                !live.observed(false),
                "newline-terminated bytes are not evidence of a harness"
            );
        }
        // So the registry is never told about `sess-forged`, and the tab keeps
        // the session it was contaminated in.
        assert_eq!(reg.snapshot()[0].session.as_deref(), Some("sess-a"));

        // Forgery 2: the attacker writes a record naming the session, clearing
        // the decode bar. The rotation reaches `observe`.
        let forged = scope("claude-1", Some("sess-forged"));
        let view = reg.view_for(&forged);
        assert_eq!(
            view.latch, "open",
            "the permissive state does reset — the fix must not freeze latches"
        );
        assert!(
            view.contaminated,
            "…and the contamination bit does not: no rotation clears an unarmed tab"
        );
        assert_eq!(
            reg.gate(
                Some(&forged),
                LatchRoute::Native,
                "context_note",
                ON,
                NO_CONTENT
            ),
            Ok(WriteTaint::Quarantined),
            "the persistence channel stays closed"
        );
        // Nor can a rotation ARM one — the only writer of the arm is a click.
        assert!(!reg.snapshot()[0].view.awaiting_session_clear);
    }

    /// **Clearing re-arms the transition report — proved by the consequence.**
    ///
    /// `latch_flagged` / `beacon_flagged` are one-row-per-scope claim bits, and
    /// the `contamination` row is self-limiting through `note_contamination`'s
    /// `mem::replace`. Leave any of them set across a clear and a tab that gets
    /// re-contaminated writes **no new row**: the feed says the tab is clean, the
    /// registry says it is not, and the only trace is the quarantine rows of
    /// later writes. That is the same class of bug #48 fixed for the
    /// `Local`-latched beacon.
    ///
    /// Asserted as "a re-contamination writes a new row", not as
    /// `assert!(!entry.beacon_flagged)`: the boolean is the mechanism, the row is
    /// the invariant, and a mechanism swapped for another one must not fail this
    /// test while a lost row must.
    ///
    /// **And both claim bits are actually SPENT first.** The obvious version of
    /// this test starts from a proxied fetch, which sets neither bit — so the
    /// clear's resets are no-ops and deleting them leaves the test green. (That
    /// was the first draft, and reverting the resets did not turn it red. It is
    /// exactly the failure mode this whole area keeps producing: a test that
    /// pins the happy path's shape rather than the invariant.) So the tab here
    /// is LOCAL-latched and it spends both: a beacon that contaminates without
    /// moving the latch, and a refused proxied call.
    #[test]
    fn a_re_contamination_after_a_clear_writes_a_new_row() {
        outbound::test_rows::reset();
        let (reg, s) = contaminated_local_registry();
        let rows = outbound::test_rows::drain();
        assert_eq!(
            contamination_rows(&rows).len(),
            1,
            "the first contamination is recorded"
        );
        // Spend `beacon_flagged`: this beacon reported, so the next one in the
        // same tab-session must not.
        for _ in 0..3 {
            assert!(!reg.beacon(Some(&s), "WebSearch", ON, BEACON_PROV).report);
        }
        // Spend `latch_flagged`: the first refusal writes a row, later ones do
        // not — that bound is what makes leaving the bit set invisible.
        for i in 0..3 {
            assert_eq!(
                reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
                Err(REFUSAL_EXTERNAL_BLOCKED)
            );
            let rows = outbound::test_rows::drain();
            let refusals = outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal);
            assert_eq!(refusals.len(), usize::from(i == 0), "refusal {i}");
        }

        reg.apply_override(&s, LatchOverride::ClearContamination)
            .expect("resume");
        // (No `contamination_cleared` row is expected from the registry here:
        // the resume's row is composed by `override_row` and written by
        // `apply_latch_override`, the IPC entry point, exactly as the two latch
        // moves' rows always have been. The same is true of the unlatch's
        // release row (decision 15's 2026-08-10 amendment), which `unlatch_clear_row`
        // composes for that same entry point — so the `Unlatch` below likewise
        // adds nothing to this feed. Both are asserted in
        // `every_clear_records_its_basis_and_the_state_it_replaced`.)
        assert!(cleared_rows(&outbound::test_rows::drain()).is_empty());

        // 1. The harness reads a page again. The conversation was clean a moment
        //    ago, so this is a NEW transition and must be reported as one — both
        //    as a contamination row and as the beacon's own row.
        let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
        assert!(out.contaminated_now, "the tab is contaminated again");
        assert!(
            out.report,
            "a beacon after a clear is a new fact — a stale `beacon_flagged` makes \
             the whole event silent, which is the #48 bug one clear later"
        );
        report_beacon(Some(&s), outbound::Origin::Http, "WebFetch", &out);
        let rows = outbound::test_rows::drain();
        assert_eq!(
            contamination_rows(&rows).len(),
            1,
            "the re-contamination writes its own transition row"
        );
        assert_eq!(
            outbound::test_rows::of_screen(&rows, outbound::Screen::LatchBeacon).len(),
            1,
            "…and the beacon row beside it"
        );

        // 2. The next refusal in the re-contaminated tab is likewise a fact the
        //    feed has not carried since the clear.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Proxied, "ddg__search", ON, NO_CONTENT),
            Err(REFUSAL_EXTERNAL_BLOCKED)
        );
        let rows = outbound::test_rows::drain();
        assert_eq!(
            outbound::test_rows::of_screen(&rows, outbound::Screen::LatchRefusal).len(),
            1,
            "a refusal after a clear must be reportable again"
        );

        // 3. And the proxied intake path, which flips the bit through a
        //    different door, reports its own re-contamination too.
        reg.apply_override(&s, LatchOverride::ClearContamination)
            .expect("resume again");
        let _ = outbound::test_rows::drain();
        assert_eq!(
            reg.gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
            ),
            Err(REFUSAL_EXTERNAL_BLOCKED),
            "the LOCAL latch still refuses it — the clear is not an unlatch"
        );
        reg.apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        let _ = outbound::test_rows::drain();
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                CallProvenance::intake(Some("https://evil2.example/p"), Some("evil2.example")),
            )
            .is_ok());
        let rows = outbound::test_rows::drain();
        let hits = contamination_rows(&rows);
        assert_eq!(hits.len(), 1, "the proxied path reports it too");
        assert_eq!(payload(hits[0])["host"], "evil2.example");
    }

    /// **Decision 10 is not touched by any of this.** Clearing the tab bit stops
    /// FUTURE writes being held; notes already quarantined stay quarantined, and
    /// promote-or-discard remains the Memory view's own review — a separate
    /// consent surface with a separate click.
    ///
    /// Two halves, because the interesting failure is a well-meaning one:
    /// someone wiring "and release this tab's held notes" into the clear.
    #[test]
    fn clearing_the_bit_does_not_promote_anything_already_quarantined() {
        // LOCAL-latched: the bit is what decides here, not the latch.
        let (reg, s) = contaminated_local_registry();
        // A note written while contaminated is held for review.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Quarantined)
        );
        reg.apply_override(&s, LatchOverride::ClearContamination)
            .expect("resume");
        // Only the NEXT write changes.
        assert_eq!(
            reg.gate(Some(&s), LatchRoute::Native, "context_note", ON, NO_CONTENT),
            Ok(WriteTaint::Clean),
            "future writes are stored clean again — that is the whole effect"
        );

        // And the structural half: nothing on the clear path can reach a stored
        // note. The note store's release/delete API is named here so that wiring
        // it into this module fails the build's own test rather than a review.
        // `concat!` throughout: a needle written whole would match its own text
        // in the file it scans.
        let src = include_str!("loopback.rs");
        for promotion in [
            concat!("mem_", "promote_note"),
            concat!("mem_", "delete_note"),
            concat!("mem_", "quarantined_notes"),
        ] {
            assert!(
                !src.contains(promotion),
                "`{promotion}` appeared in loopback.rs — promoting a quarantined note is \
                 the Memory view's own review (locked decision 10), not a side effect of \
                 clearing a tab's contamination flag"
            );
        }
    }

    /// **The audit row: basis, prior state, and who acted** — for both clears,
    /// because they are the same state change reached two ways and a reviewer
    /// must be able to tell them apart.
    #[test]
    fn every_clear_records_its_basis_and_the_state_it_replaced() {
        // Half 1: the immediate resume. Origin `ipc` — a human, right now.
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();
        let out = reg
            .apply_override(&s, LatchOverride::ClearContamination)
            .expect("resume");
        let row = override_row(
            outbound::Origin::Ipc,
            LatchOverride::ClearContamination,
            &out,
        );
        assert_eq!(
            row.screen,
            outbound::Screen::ContaminationCleared,
            "a clear is filed beside the row that SET the bit, not among latch moves"
        );
        assert_eq!(row.tool, "clear_contamination");
        assert_eq!(row.origin, outbound::Origin::Ipc);
        let d = &row.detail;
        assert!(d.contains("basis: clear_contamination"), "{d}");
        assert!(d.contains("origin: ipc"), "{d}");
        assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
        assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
        assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
        assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");

        // Half 2: the armed rotation. The row is written by the registry itself
        // (nothing else observes the rotation), so it is asserted through the
        // feed rather than through a builder.
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();
        reg.apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect("arm");
        let armrows = outbound::test_rows::drain();
        let arm = outbound::test_rows::of_screen(&armrows, outbound::Screen::LatchOverride);
        assert_eq!(
            arm.len(),
            0,
            "the arm row is written by the IPC entry point"
        );
        assert!(
            cleared_rows(&armrows).is_empty(),
            "arming clears nothing, so it writes no clear row"
        );

        let rotated = scope("claude-1", Some("sess-b"));
        assert!(!reg.view_for(&rotated).contaminated);
        let rows = outbound::test_rows::drain();
        let hits = cleared_rows(&rows);
        assert_eq!(hits.len(), 1, "the armed clear writes exactly one row");
        let hit = hits[0];
        assert_eq!(hit.entry.tool, "session_clear_observed");
        assert_eq!(hit.entry.root, TEST_ROOT, "an empty root defeats the row");
        assert!(hit.entry.ok, "nothing was denied");
        let req = payload(hit);
        assert_eq!(req["screen"], "contamination_cleared");
        assert_eq!(
            req["origin"], "internal",
            "the trigger is cImp's own observation; `ipc` means a human acted NOW"
        );
        assert_eq!(req["scope"], "claude:claude-1");
        assert_eq!(
            req["session"], "sess-a",
            "filed under the CONTAMINATED conversation, so it joins the row that opened it"
        );
        let d = &hit.response;
        assert!(d.contains("basis: session_clear_observed"), "{d}");
        assert!(d.contains("ONE-SHOT"), "{d}");
        assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
        assert!(d.contains("(sess-b)"), "and the one that replaced it: {d}");
        assert!(d.contains("latch=external"), "the PRIOR latch: {d}");

        // Half 3: the full unlatch (decision 15's 2026-08-10 amendment). Same
        // shape as half 1 — asserted through the builder, because this row is
        // likewise composed for `apply_latch_override` to file — and it must be
        // a DIFFERENT basis from the resume: "that content was harmless" and "I
        // am taking the whole risk knowingly" are different claims, and a
        // reviewer who cannot tell them apart cannot reconstruct the decision.
        outbound::test_rows::reset();
        let (reg, s) = contaminated_registry();
        let out = reg
            .apply_override(&s, LatchOverride::Unlatch)
            .expect("unlatch");
        let row = unlatch_clear_row(outbound::Origin::Ipc, LatchOverride::Unlatch, &s, &out)
            .expect("the release owes its own row");
        assert_eq!(row.origin, outbound::Origin::Ipc, "a human acted NOW");
        assert_eq!(row.basis.tool(), "unlatch");
        assert_ne!(row.basis, ClearBasis::Resume);
        assert_eq!(row.scope, "claude:claude-1");
        assert_eq!(row.session.as_deref(), Some("sess-a"));
        let d = &row.detail;
        assert!(d.contains("basis: unlatch"), "{d}");
        assert!(d.contains("origin: ipc"), "{d}");
        assert!(d.contains("contaminated=true"), "the PRIOR state: {d}");
        assert!(d.contains("latch=external"), "the PRIOR latch: {d}");
        assert!(d.contains("session=sess-a"), "the PRIOR session: {d}");
        assert!(d.contains("STAY quarantined"), "decision 10 stated: {d}");
        assert!(
            d.contains("moved the latch to `open`"),
            "the one sentence the three bases cannot share: {d}"
        );
    }

    /// **The arm's own row.** It is not a clear, so it is filed as a latch
    /// override — and it has to say, in words, that the flag is still set, or a
    /// reader who sees "restore" and no later `contamination_cleared` row cannot
    /// tell "still waiting" from "lost".
    #[test]
    fn the_restore_arm_writes_a_row_that_says_the_flag_is_still_set() {
        let (reg, s) = contaminated_registry();
        let out = reg
            .apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect("arm");
        let row = override_row(
            outbound::Origin::Ipc,
            LatchOverride::AwaitSessionClear,
            &out,
        );
        assert_eq!(row.screen, outbound::Screen::LatchOverride);
        assert_eq!(row.tool, "await_session_clear");
        let d = &row.detail;
        assert!(d.contains("NOT cleared"), "{d}");
        assert!(d.contains("contaminated=true"), "{d}");
        assert!(d.contains("`/clear`"), "the user is told what to do: {d}");
    }

    // ── #48, finding M-7 — the route enumeration, by containment property ──
    //
    // The finding's third clause was that the three `/context/*` hook routes
    // "appear in no route enumeration". They did appear in the pinned path list
    // below — what they appeared in was an enumeration of STRINGS, which cannot
    // tell a route that gates from a route that walks straight into
    // `GraphService`. So the enumeration now records, per route, what it does
    // about the taint latch, and the test checks that claim against the
    // handler's own source rather than restating it.

    /// What one HTTP route does about the V32 session taint latch.
    #[derive(Debug)]
    enum Containment {
        /// Gates, on a tool name the REQUEST supplies (`/run`, `/graph_run`,
        /// `/audit/run`, `/mcp/call`). Which class the call lands in is the
        /// caller's tool's business; that the registry is consulted at all is
        /// this route's.
        GatesRequestTool,
        /// Gates, on a FIXED [`toolclass::TABLE`] name — the three hook routes.
        /// `refused_under_external` states the security-relevant consequence:
        /// whether a conversation that has ingested untrusted content is
        /// REFUSED here. It is checked against `toolclass`, not restated, so a
        /// demotion of the row shows up as a failure of this test.
        GatesFixedTool {
            tool: &'static str,
            refused_under_external: bool,
        },
        /// Touches the registry for something that is not a capability gate —
        /// a state read, or the beacon (which can only ever tighten). The
        /// string says which.
        RegistryNoGate(&'static str),
        /// Never consults the registry. The string is the reason, and it is the
        /// claim a reviewer has to disagree with in order to add a route here.
        NoRegistry(&'static str),
    }

    struct RouteRow {
        path: &'static str,
        method: &'static str,
        /// The handler function the dispatch table routes to. `""` for the two
        /// routes answered inline in the dispatch arm itself.
        handler: &'static str,
        containment: Containment,
    }

    const fn route(
        path: &'static str,
        method: &'static str,
        handler: &'static str,
        containment: Containment,
    ) -> RouteRow {
        RouteRow {
            path,
            method,
            handler,
            containment,
        }
    }

    /// **Every route this listener serves, and what it does about containment.**
    ///
    /// This is the single enumeration: `no_http_route_can_reach_a_contamination_clear`
    /// pins the SURFACE from it and
    /// `every_loopback_route_declares_what_it_does_about_the_latch` pins the
    /// PROPERTY. A new route therefore cannot be added by editing one list.
    const ROUTE_CONTAINMENT: &[RouteRow] = &[
        route("/run", "POST", "handle_run", Containment::GatesRequestTool),
        route(
            "/graph_run",
            "POST",
            "handle_graph_run",
            Containment::GatesRequestTool,
        ),
        route(
            "/audit/run",
            "POST",
            "handle_audit_run",
            Containment::GatesRequestTool,
        ),
        // RECORDED RESIDUAL, not a clean case. The auto-injection channel: its
        // V32 containment is the spotlighting / memory-quarantine envelope
        // (locked decisions 10/12), not refusal, and it fires on every prompt
        // rather than on a model's election. But its digest carries exported
        // SIGNATURES — the same content H-1 demoted `graph_repo_map` for — so
        // "ungated" here is a standing question, not a settled one. M-7 named
        // three routes and this is not one of them; it is written down so the
        // next reviewer inherits the question instead of rediscovering it.
        route(
            "/context/retrieve",
            "POST",
            "handle_context_retrieve",
            Containment::NoRegistry(
                "auto-injection; contained by the spotlight/quarantine envelope, not by refusal",
            ),
        ),
        // V33 Phase F. Takes a Workbench checkpoint before a mutating tool
        // call. Ungated on purpose, and the reason is not "nobody got round to
        // it": it returns two booleans, grants no capability and hands back no
        // project data, so a latch would have nothing to refuse — while a
        // refusal would remove checkpoints from a tab that had touched external
        // content, i.e. exactly the tab most likely to need a rewind. It is
        // still token-gated, tab-narrowed, throttled per `(root, tab)` and
        // re-checked against the class table for `mutates_fs`.
        route(
            "/workbench/tool_checkpoint",
            "POST",
            "handle_tool_checkpoint",
            Containment::NoRegistry(
                "snapshots the work tree into cImp's own shadow repo; returns no local data \
                 and grants no capability",
            ),
        ),
        route(
            "/context/compaction",
            "POST",
            "handle_context_compaction",
            Containment::GatesFixedTool {
                tool: HOOK_TOOL_COMPACTION,
                // TRUSTED: paths, symbol names and memory-note text, no source
                // text. Gated so the class table stays the one place this can
                // change.
                refused_under_external: false,
            },
        ),
        route(
            "/context/should_read",
            "POST",
            "handle_should_read",
            Containment::GatesFixedTool {
                tool: HOOK_TOOL_SHOULD_READ,
                refused_under_external: true,
            },
        ),
        route(
            "/context/post_edit",
            "POST",
            "handle_post_edit",
            Containment::GatesFixedTool {
                tool: HOOK_TOOL_POST_EDIT,
                refused_under_external: true,
            },
        ),
        route(
            "/memory/event",
            "POST",
            "handle_memory_event",
            // Ingress, not egress: it records the caller's OWN tool/usage events
            // and returns no project data. Nothing to refuse — a latch here
            // would only lose the record of what the tab did.
            Containment::NoRegistry("records the caller's own events; returns no local data"),
        ),
        route(
            "/activity/contract_drift",
            "POST",
            "handle_contract_drift",
            Containment::NoRegistry("a shim reporting its own broken payload; returns nothing"),
        ),
        route(
            // The constant the CHILD sends, so the two ends cannot drift: the
            // dispatch arm is scanned from the source and compared against this
            // list, which is built from `DISCOVERY_SKIPPED_PATH`.
            DISCOVERY_SKIPPED_PATH,
            "POST",
            "handle_discovery_skipped",
            Containment::NoRegistry(
                "a child reporting a discovery entry it skipped; answers a fixed `ok` on every \
                 path and touches no registry",
            ),
        ),
        route(
            "/permission/event",
            "POST",
            "handle_permission_event",
            Containment::NoRegistry("a hook reporting a permission prompt; returns nothing"),
        ),
        route(
            "/latch/beacon",
            "POST",
            "handle_latch_beacon",
            Containment::RegistryNoGate(
                "engages the EXTERNAL latch for a harness-native web tool; it can only tighten",
            ),
        ),
        route(
            "/latch/state",
            "POST",
            "handle_latch_state",
            Containment::RegistryNoGate(
                "reads this tab's view for the plugin gate; creates nothing",
            ),
        ),
        // V35 Phase I (CHP). A generated harness artifact declaring its protocol
        // version and what it will serve. Ungated on purpose and the reason is
        // not "nobody got round to it": it hands back no project data and grants
        // no capability, so a latch would have nothing to refuse — while a
        // refusal would deny cImp the one message that says a tab is running a
        // STALE plugin, i.e. exactly the tab whose containment behaviour is in
        // question. It is still token-gated, and its tab id is validated against
        // the user's configured AI tabs before anything is recorded.
        route(
            "/session/hello",
            "POST",
            "handle_session_hello",
            Containment::NoRegistry(
                "a harness artifact declaring its protocol version and capabilities; returns no \
                 local data and grants nothing",
            ),
        ),
        route(
            "/mcp/list",
            "POST",
            "handle_mcp_list",
            // Advertisement only. Consumers cache `tools/list` at connect, which
            // is exactly why the proxy enforces by REFUSAL at `/mcp/call`
            // instead of by removing defs here (decision 3).
            Containment::NoRegistry("tool advertisement; enforcement is by refusal at /mcp/call"),
        ),
        route(
            "/mcp/call",
            "POST",
            "handle_mcp_call",
            Containment::GatesRequestTool,
        ),
        route(
            "/describe",
            "GET",
            "",
            Containment::NoRegistry("the proxy's own tool list as text; no project data"),
        ),
        route(
            "/events",
            "GET",
            "handle_events",
            Containment::NoRegistry("the offload service's own event stream"),
        ),
        route(
            "/health",
            "GET",
            "",
            Containment::NoRegistry("a fixed `ok`"),
        ),
        route(
            "/status",
            "GET",
            "handle_status",
            // Reads latch state through `latch_snapshot`, not through the
            // registry handle — a debug view over cImp's own identifiers, with
            // no capability behind it.
            Containment::NoRegistry("a debug view of cImp's own identifiers"),
        ),
    ];

    /// Every path the dispatch table routes, sorted and deduped — scanned from
    /// the source because the `match` is not reachable from a test.
    fn dispatched_routes(src: &str) -> Vec<&str> {
        let mut routes: Vec<&str> = Vec::new();
        for marker in ["(\"POST\", \"", "(\"GET\", \""] {
            for part in src.split(marker).skip(1) {
                routes.push(part.split('"').next().expect("a closing quote"));
            }
        }
        routes.sort_unstable();
        routes.dedup();
        routes
    }

    /// The source text of one top-level `async fn`, signature to closing brace,
    /// with line endings normalised to `\n` (this file is checked out CRLF on
    /// Windows, and a needle written with `\n` would silently match nothing —
    /// which for a security assertion means silently passing).
    ///
    /// Starts at the SIGNATURE, so a handler's doc comment is deliberately not
    /// part of it: a route must not be able to claim a gate in prose.
    fn handler_body(src: &str, name: &str) -> String {
        let sig = format!("async fn {name}(");
        let mut out = String::new();
        let mut inside = false;
        for line in src.lines() {
            if !inside {
                if !line.starts_with(&sig) {
                    continue;
                }
                inside = true;
            }
            out.push_str(line);
            out.push('\n');
            // The closing brace of a top-level item is the only `}` in column 0.
            if line == "}" {
                break;
            }
        }
        assert!(!out.is_empty(), "no top-level `async fn {name}`");
        assert!(
            out.ends_with("}\n"),
            "`async fn {name}` was not terminated — the scan would read past it"
        );
        out
    }

    /// **M-7's third clause.** Every route the listener serves declares what it
    /// does about the taint latch, and the declaration is checked against the
    /// handler rather than believed.
    ///
    /// The four checks, and what each one catches:
    ///
    /// 1. Every dispatched path is declared, and every declared path is
    ///    dispatched — so a new route cannot slip in unclassified.
    /// 2. A route that claims to gate must actually reach `latches()`. **This
    ///    is the check that would have failed before this commit** for the
    ///    three `/context/*` hooks, and it is what stops the classic failure of
    ///    a gate tested through its helper while the call site is deleted.
    /// 3. A route that claims NOT to touch the registry must not — so a gate
    ///    added without a review of what it means to that route also fails.
    /// 4. A fixed-tool route names a real class-table row, uses that constant
    ///    in its own body, and the declared "refused under EXTERNAL" answer is
    ///    computed from [`toolclass`], not restated. Demoting
    ///    `hook_post_edit` to TRUSTED therefore fails here.
    #[test]
    fn every_loopback_route_declares_what_it_does_about_the_latch() {
        let src = include_str!("loopback.rs");

        // 1. Surface ↔ declaration, both directions.
        let mut declared: Vec<&str> = ROUTE_CONTAINMENT.iter().map(|r| r.path).collect();
        declared.sort_unstable();
        assert_eq!(
            dispatched_routes(src),
            declared,
            "a route is dispatched but undeclared (or the reverse)"
        );

        for row in ROUTE_CONTAINMENT {
            // The declared handler really is the one the dispatch routes to.
            let arm = format!("(\"{}\", \"{}\") =>", row.method, row.path);
            let arm_at = src
                .find(&arm)
                .unwrap_or_else(|| panic!("no dispatch arm for {}", row.path));
            if !row.handler.is_empty() {
                assert!(
                    src[arm_at..].starts_with(&format!("{arm} {}(", row.handler)),
                    "{} does not dispatch to `{}`",
                    row.path,
                    row.handler
                );
            }
            // The two inline arms have no handler to scan; nothing behind them
            // can gate, which is why they are the only rows allowed to omit one.
            if row.handler.is_empty() {
                assert!(
                    matches!(row.containment, Containment::NoRegistry(_)),
                    "{} is answered inline, so it cannot be gating anything",
                    row.path
                );
                continue;
            }
            let body = handler_body(src, row.handler);
            let reaches_registry = body.contains("latches()");
            let gates = body.contains("latches().gate(")
                || body.contains("hook_admit(\n        latches(),")
                || body.contains("audit_admit(\n        latches(),");

            match row.containment {
                Containment::GatesRequestTool => assert!(
                    gates,
                    "{} claims to gate but its handler never reaches the latch registry",
                    row.path
                ),
                Containment::GatesFixedTool {
                    tool,
                    refused_under_external,
                } => {
                    assert!(
                        gates,
                        "{} claims to gate but its handler never reaches the latch registry",
                        row.path
                    );
                    assert!(
                        body.contains(tool_const(tool)),
                        "{} must gate on `{tool}`'s constant in its own body",
                        row.path
                    );
                    // The security-relevant property, computed rather than
                    // restated: is a contaminated conversation refused here?
                    assert_eq!(
                        Latch::External.blocks(toolclass::classify(tool)),
                        refused_under_external,
                        "`{tool}`'s class no longer matches what {} declares",
                        row.path
                    );
                }
                Containment::RegistryNoGate(why) => {
                    assert!(
                        reaches_registry,
                        "{} claims to reach the registry ({why}) and does not",
                        row.path
                    );
                    assert!(
                        !gates,
                        "{} now gates capability — declare it, don't leave it as a state read",
                        row.path
                    );
                }
                Containment::NoRegistry(why) => assert!(
                    !reaches_registry,
                    "{} is declared ungated ({why}) but now reaches the latch registry",
                    row.path
                ),
            }
        }
    }

    /// The identifier a hook tool-name constant is written as at the call site.
    /// The handler bodies use the CONSTANT, not the string, so the check above
    /// has to look for the same thing a reader would.
    fn tool_const(tool: &str) -> &'static str {
        match tool {
            "hook_post_edit" => "HOOK_TOOL_POST_EDIT",
            "hook_should_read" => "HOOK_TOOL_SHOULD_READ",
            "hook_compaction" => "HOOK_TOOL_COMPACTION",
            other => panic!("no constant known for `{other}`"),
        }
    }

    /// **M-7's first clause: an EXTERNAL-latched tab reaches local capability
    /// through these routes.** Now it does not.
    ///
    /// `post_edit` executes the project's configured check commands and
    /// `should_read` hands back repo source text, so a conversation that has
    /// ingested untrusted content is refused both. The compaction carry-over is
    /// admitted, and that is stated here rather than left to be inferred from a
    /// missing assertion — it is TRUSTED content (paths, symbol names, note
    /// text) and refusing it would also skip the route's dedup-clear side
    /// effects.
    #[test]
    fn a_contaminated_conversation_is_refused_the_executing_hook_routes() {
        let reg = LatchRegistry::default();
        let s = scope("claude-1", Some("sess-a"));
        // One proxied fetch contaminates the conversation.
        assert!(reg
            .gate(
                Some(&s),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(reg.snapshot()[0].latch(), "external");

        let admit = |tool: &'static str| {
            hook_admit(
                &reg,
                tool,
                "claude",
                Some("claude-1"),
                |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
                |_| ON,
            )
        };
        assert_eq!(
            admit(HOOK_TOOL_POST_EDIT),
            Err(REFUSAL_LOCAL_BLOCKED),
            "a contaminated conversation must not have the project's checks executed for it"
        );
        assert_eq!(
            admit(HOOK_TOOL_SHOULD_READ),
            Err(REFUSAL_LOCAL_BLOCKED),
            "…nor be handed repo source text by the read advisor"
        );
        assert_eq!(
            admit(HOOK_TOOL_COMPACTION),
            Ok(()),
            "the carry-over is TRUSTED content and stays admitted"
        );
        // A refused hook never redefines which side of the boundary the
        // conversation is on.
        assert_eq!(reg.snapshot()[0].latch(), "external");
    }

    /// **A hook may be refused by a latch but must never move one.**
    ///
    /// This is what [`LatchRoute::Hook`] exists for, and getting it wrong would
    /// have been worse than the hole: `post_edit`/`should_read` classify
    /// LOCAL-CAPABILITY, so gating them on `LatchRoute::Native` would latch
    /// every tab with the read advisor or auto-check on to `Local` at its first
    /// read or edit — silently refusing every proxied web/MCP tool for the rest
    /// of the session, for a choice the model never made.
    ///
    /// The `Native` half of the assertion is the control, so this test cannot
    /// pass by the gate having done nothing at all. **It changed with #48's
    /// M-2 fix and the change is the finding**: the control used to be the SAME
    /// NAME on `LatchRoute::Native`, which latched — and M-7's own review
    /// recorded that as a residual, because `hook_post_edit` is not a tool a
    /// model can call, so a model that emits it has hallucinated and used to
    /// cost its tab every proxied tool for the session. The control is now a
    /// name that really is elective and really dispatches, and the old case is
    /// asserted the other way round beside it.
    #[test]
    fn a_hook_route_reads_the_latch_and_never_engages_it() {
        let reg = LatchRegistry::default();
        for tool in [HOOK_TOOL_POST_EDIT, HOOK_TOOL_SHOULD_READ] {
            assert_eq!(
                hook_admit(
                    &reg,
                    tool,
                    "claude",
                    Some("claude-1"),
                    |_, _| LatchScoping::Scoped(scope("claude-1", Some("sess-a"))),
                    |_| ON,
                ),
                Ok(())
            );
        }
        assert_eq!(
            reg.snapshot()[0].latch(),
            "open",
            "the hooks fired on cImp's own automation — the conversation elected nothing"
        );
        // …and the proxied web side is therefore still available, which is the
        // user-visible fact the previous assertion protects.
        assert!(reg
            .gate(
                Some(&scope("claude-1", Some("sess-a"))),
                LatchRoute::Proxied,
                "ddg__search",
                ON,
                NO_CONTENT
            )
            .is_ok());

        // The control: a name that IS elective and IS dispatchable latches on
        // the same route, with the same registry and scope shape.
        let elective = LatchRegistry::default();
        assert!(elective
            .gate(
                Some(&scope("claude-2", Some("sess-b"))),
                LatchRoute::Native,
                "graph_snippet",
                ON,
                NO_CONTENT
            )
            .is_ok());
        assert_eq!(elective.snapshot()[0].latch(), "local");

        // …and the case that used to be the control, now asserted the other way
        // round (#48, M-2): `hook_post_edit` arriving as a MODEL's tool call is
        // a hallucination — no dispatcher serves that name — so it neither
        // latches nor is refused, and the tab keeps its tools.
        let hallucinated = LatchRegistry::default();
        assert_eq!(
            hallucinated.gate(
                Some(&scope("claude-3", Some("sess-c"))),
                LatchRoute::Native,
                HOOK_TOOL_POST_EDIT,
                ON,
                NO_CONTENT
            ),
            Ok(WriteTaint::Clean)
        );
        assert!(
            hallucinated.snapshot().is_empty(),
            "one hallucinated name must not cost a tab its web tools (A-1's harm, M-2's half)"
        );
    }

    /// The residual, pinned so it is a decision and not an accident: a hook POST
    /// with no usable tab identity resolves no scope and is ADMITTED.
    ///
    /// That is the locked fail-open posture of `latch_scope` (a shim from a
    /// build before `--tab` was baked in must not lose the feature), and it is
    /// what finding F-5/H-8 tracks. Pinned here so that a future change to it is
    /// a deliberate edit to this test, and so the residual cannot be read as
    /// "someone forgot".
    #[test]
    fn a_hook_post_without_a_tab_is_admitted_and_keys_nothing() {
        for scoping in [
            LatchScoping::Anonymous,
            LatchScoping::Unknown("ghost".into()),
        ] {
            let reg = LatchRegistry::default();
            // Contaminate a real tab first: the point is that the ungated call
            // is ungated because it has no identity, not because nothing was
            // latched anywhere.
            assert!(reg
                .gate(
                    Some(&scope("claude-1", Some("sess-a"))),
                    LatchRoute::Proxied,
                    "ddg__fetch_content",
                    ON,
                    NO_CONTENT
                )
                .is_ok());
            assert_eq!(
                hook_admit(
                    &reg,
                    HOOK_TOOL_POST_EDIT,
                    "claude",
                    None,
                    |_, _| scoping,
                    |_| ON,
                ),
                Ok(())
            );
            // #45's bound: no identity ⇒ no registry row of its own.
            assert_eq!(
                reg.snapshot().len(),
                1,
                "only the contaminated tab is keyed"
            );
        }
    }

    /// **V33 C4 (finding F-5's directory half): `/context/post_edit` executes
    /// the project's configured check commands, and it will only do so in a
    /// directory this instance serves.**
    ///
    /// The `cwd` used to come straight out of the request body (defaulting to
    /// `"."`) with no ancestor check and no allowlist, so a token-holder could
    /// have the operator's own vetted commands run in a directory it named.
    ///
    /// This exercises the decision function, which is pure so the property is
    /// assertable without a `TcpStream` or an `AppHandle` — the [`audit_admit`]
    /// shape, and the same two path helpers that route's step 3 uses.
    ///
    /// **What this would still pass with, and the guards:** a check that only
    /// compared string prefixes (`P:\projx` is asserted to be refused — it is a
    /// prefix of neither root component-wise, which is the trap
    /// [`is_ancestor_or_equal`] exists for); a check that canonicalized and
    /// therefore silently re-bucketed every existing caller (the admitted path
    /// is asserted to come back byte-for-byte as written, because it keys the
    /// single-flight runner downstream); and a component walk that trusted
    /// [`canon`] to resolve `..` (it cannot for a path that does not exist, so
    /// `..` is refused outright and that case is asserted).
    #[test]
    fn post_edit_runs_only_in_a_directory_this_instance_serves() {
        // Built with `join` rather than written with separators so the component
        // walk means the same thing on both platforms.
        let served = PathBuf::from("P:\\proj");
        let worktree = PathBuf::from("P:\\worktrees").join("feature-a");
        let roots = vec![served.clone(), worktree.clone()];
        let s = |p: &Path| p.to_string_lossy().into_owned();

        // 1. No `cwd` on the wire ⇒ the served root, never the process cwd.
        for absent in [None, Some(""), Some("   "), Some("\t")] {
            assert_eq!(
                admitted_hook_root(&roots, absent),
                Some(served.clone()),
                "{absent:?}"
            );
        }

        // 2. Inside a served root — the root itself, a subdirectory, and the
        //    same for a tab that lives in a worktree outside the launch root.
        for ok in [
            served.clone(),
            served.join("src"),
            served.join("src").join("deep"),
            worktree.clone(),
            worktree.join("src"),
        ] {
            let asked = s(&ok);
            assert_eq!(
                admitted_hook_root(&roots, Some(asked.as_str())),
                Some(PathBuf::from(&asked)),
                "{asked} is served and must come back exactly as written"
            );
        }

        // 3. Outside every root — including the string-prefix near miss, and a
        //    traversal that `canon` cannot resolve because the path does not
        //    exist (which is precisely when a component walk would be fooled).
        for bad in [
            PathBuf::from("Q:\\evil"),
            PathBuf::from("P:\\projx"),
            PathBuf::from("P:\\projx").join("src"),
            PathBuf::from("P:\\worktrees"),
            served.join("..").join("..").join("evil"),
            PathBuf::from("..").join("evil"),
        ] {
            let asked = s(&bad);
            assert_eq!(
                admitted_hook_root(&roots, Some(asked.as_str())),
                None,
                "{asked} is not served and the checks must not run there"
            );
        }

        // 4. No resolvable root at all ⇒ deny, including the absent-`cwd` case.
        //    A root that cannot be resolved reads as absent, never as "allow".
        assert_eq!(admitted_hook_root(&[], None), None);
        assert_eq!(admitted_hook_root(&[], Some(&s(&served))), None);

        // 5. Windows only: on-disk casing and an agent-reported cwd routinely
        //    disagree, which is why `is_ancestor_or_equal` folds case there.
        if cfg!(windows) {
            let shouty = s(&served).to_uppercase();
            assert_eq!(
                admitted_hook_root(&roots, Some(shouty.as_str())),
                Some(PathBuf::from(&shouty))
            );
        }
    }

    /// **V33 C4's other half, checked against the source rather than believed:
    /// the roots cannot come from the request.**
    ///
    /// The allowlist above is only as good as what feeds it, and "roots derive
    /// from configured tabs and the served root, never from the request" is the
    /// kind of claim that survives its own violation if it lives only in prose.
    /// Two structural assertions:
    ///
    /// 1. The handler resolves its working directory through the pair, so the
    ///    check cannot be deleted while the route keeps running commands.
    /// 2. [`hook_exec_roots`] takes the `AppHandle` and the settings snapshot
    ///    and NOTHING ELSE — the request is not in scope, so no future edit can
    ///    let a body widen the allowlist without changing this signature.
    #[test]
    fn post_edit_takes_its_working_directory_from_the_app_not_from_the_body() {
        let src = include_str!("loopback.rs");
        let body = handler_body(src, "handle_post_edit");
        assert!(
            body.contains("admitted_hook_root(&hook_exec_roots(app, &settings), body.cwd.as_deref())"),
            "the route must resolve its cwd through the C4 allowlist: {body}"
        );
        assert!(
            !body.contains("PathBuf::from(\".\")"),
            "the pre-V33 caller-supplied default is back: {body}"
        );
        assert!(
            src.contains(
                "fn hook_exec_roots(app: &AppHandle, settings: &crate::settings::Settings) -> Vec<PathBuf>"
            ),
            "the roots must derive from the app and the settings, never from a request body"
        );
    }

    /// `agent` is caller-asserted and absent on a pre-#48 shim. Absent ⇒
    /// `claude`, because all three Claude hooks are installed from Claude's own
    /// settings overlay; `opencode` is the only other answer, and it is the one
    /// the generated plugin's `post_edit` POST sends.
    #[test]
    fn a_hook_bodys_agent_narrows_to_the_two_that_exist() {
        assert_eq!(hook_agent(None), "claude");
        assert_eq!(hook_agent(Some("claude")), "claude");
        assert_eq!(hook_agent(Some("opencode")), "opencode");
        assert_eq!(hook_agent(Some("OpenCode")), "opencode");
        // Anything invented lands on `claude` rather than inventing an agent —
        // `source_for_consumer`'s locked behaviour, shared verbatim with
        // `/graph_run`'s `consumer`. Padding is NOT trimmed (that narrowing
        // lives in `audit_consumer`, whose route requires identity); no shim
        // sends any, and trimming would buy nothing here because `agent` is
        // caller-asserted either way — F-4 still holds, the (agent, tab) pair
        // is verified on no route, and this fix neither relies on it nor makes
        // it worse.
        assert_eq!(hook_agent(Some("offload")), "claude");
        assert_eq!(hook_agent(Some(" opencode ")), "claude");
    }

    /// All three hook bodies still parse without the two new fields — a shim or
    /// plugin file from an older build must not start failing at the parse
    /// boundary and lose the feature outright.
    #[test]
    fn pre_48_hook_bodies_still_parse_without_tab_or_agent() {
        let compaction: ContextCompactionBody =
            serde_json::from_slice(br#"{"cwd":"P:\\p","session_id":"s","trigger":"auto"}"#)
                .expect("pre-#48 compaction body");
        assert!(compaction.tab.is_none() && compaction.agent.is_none());

        let read: ShouldReadBody =
            serde_json::from_slice(br#"{"cwd":"P:\\p","session_id":"s","file_path":"a.rs"}"#)
                .expect("pre-#48 should_read body");
        assert!(read.tab.is_none() && read.agent.is_none());

        let edit: ContextPostEditBody = serde_json::from_slice(
            br#"{"cwd":"P:\\p","session_id":"s","file_path":"a.rs","tool_name":"Edit"}"#,
        )
        .expect("pre-#48 post_edit body");
        assert!(edit.tab.is_none() && edit.agent.is_none());

        // …and the new fields do arrive when sent.
        let edit: ContextPostEditBody = serde_json::from_slice(
            br#"{"session_id":"s","file_path":"a.rs","tab":"claude-1","agent":"opencode"}"#,
        )
        .expect("post-#48 post_edit body");
        assert_eq!(edit.tab.as_deref(), Some("claude-1"));
        assert_eq!(edit.agent.as_deref(), Some("opencode"));
    }

    /// **Neither clear is reachable over HTTP**, which is the invariant the whole
    /// design rests on: a model with a shell that could POST its way to a clear
    /// would defeat every part of this.
    ///
    /// Three independent halves, because each closes a different door.
    #[test]
    fn no_http_route_can_reach_a_contamination_clear() {
        // 1. The HTTP surface, pinned. Every route this listener serves is
        //    listed here; a new one fails this test until someone names it, and
        //    the point of naming it is to notice if it is an override door.
        //    (#45 removed `POST /latch/override` for exactly this reason.)
        //
        //    #48 (M-7): the list is no longer a literal here — it is
        //    [`ROUTE_CONTAINMENT`], which is the same enumeration answering one
        //    more question per route (does it gate?). ONE list, so a new route
        //    cannot satisfy one enumeration and be missing from the other.
        let src = include_str!("loopback.rs");
        let routes = dispatched_routes(src);
        let declared: Vec<&str> = {
            let mut v: Vec<&str> = ROUTE_CONTAINMENT.iter().map(|r| r.path).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            routes, declared,
            "the loopback's HTTP surface changed — is the new route a door onto the \
             latch override or the contamination clear?"
        );

        // 2. The only entry point that can clear is not an HTTP handler. Its
        //    doc says so; this asserts the shape the doc describes — the three
        //    clearing actions (`clear_contamination`, `unlatch` since decision
        //    15's 2026-08-10 amendment, and the deferred `await_session_clear`)
        //    exist solely as `LatchOverride` values, and the only function that
        //    turns a string into one is called from the IPC command.
        let ipc = include_str!("../ipc/commands.rs");
        assert!(
            ipc.contains("apply_latch_override(&app, &consumer, &tab, &action)"),
            "the IPC command is the caller of record"
        );
        // `concat!` so this needle does not match itself in the source it
        // scans — the first version of this assertion counted 2 and was
        // "failing" on nothing but its own text.
        assert_eq!(
            src.matches(concat!("pub fn ", "apply_latch_override"))
                .count(),
            1,
            "one entry point, or the doc's claim is unverifiable"
        );
        assert!(
            !src.contains(concat!("LatchOverride::", "parse(&body"))
                && !src.contains(concat!("LatchOverride::", "parse(body")),
            "an override action parsed from a request body is an HTTP door"
        );

        // 3. Behaviourally: the two registry entry points that ARE HTTP-reachable
        //    (`/latch/beacon` → `beacon`, `/latch/state` → `view_for`) can
        //    neither clear an unarmed tab nor arm one. The beacon only ever
        //    tightens, and that must not have widened.
        let (reg, s) = contaminated_registry();
        for _ in 0..5 {
            let out = reg.beacon(Some(&s), "WebFetch", ON, BEACON_PROV);
            assert!(out.view.contaminated, "a beacon cannot clear");
            assert!(!out.view.awaiting_session_clear, "a beacon cannot arm");
        }
        // …including across a rotation, which is the one moment an arm would
        // matter. Nothing an HTTP caller can send sets it.
        let rotated = scope("claude-1", Some("sess-b"));
        assert!(reg.view_for(&rotated).contaminated);
        assert!(
            reg.beacon(Some(&rotated), "WebFetch", ON, BEACON_PROV)
                .view
                .contaminated
        );
    }

    /// The registry's read path folds live sessions in, so an armed one-shot
    /// fires on the UI's existing 4 s poll rather than waiting for the model to
    /// call a cImp tool.
    ///
    /// `latch_snapshot` itself needs an `AppHandle` (it resolves the live-session
    /// registry), which this crate cannot mock — so what is asserted here is the
    /// half that has the logic: given resolved scopes, `observe_all` applies the
    /// same rotation rule to every entry and hands back the rows to record.
    #[test]
    fn the_read_path_observes_rotations_for_every_tab_it_reports() {
        let (reg, s) = contaminated_registry();
        reg.apply_override(&s, LatchOverride::AwaitSessionClear)
            .expect("arm");

        // A second, unarmed tab in the same registry: it must be observed too
        // (its latch reopens) and cleared not at all.
        let other = scope("claude-2", Some("sess-x"));
        assert!(reg
            .gate(
                Some(&other),
                LatchRoute::Proxied,
                "ddg__fetch_content",
                ON,
                NO_CONTENT
            )
            .is_ok());

        let keys = reg.keys();
        assert_eq!(keys.len(), 2, "both tabs are in the registry");
        let rotated = [
            scope("claude-1", Some("sess-b")),
            scope("claude-2", Some("sess-y")),
        ];
        let cleared = reg.observe_all(&rotated);
        assert_eq!(cleared.len(), 1, "exactly the armed tab clears");

        let rows = reg.snapshot();
        let armed = rows.iter().find(|r| r.tab == "claude-1").expect("claude-1");
        let unarmed = rows.iter().find(|r| r.tab == "claude-2").expect("claude-2");
        assert!(!armed.view.contaminated);
        assert!(
            unarmed.view.contaminated,
            "the read path must not have become a second way to un-taint a tab"
        );
        assert_eq!(unarmed.latch(), "open", "…while still resetting the latch");
        assert_eq!(unarmed.session.as_deref(), Some("sess-y"));

        // An entry the caller resolved no scope for is simply skipped; it is not
        // an error and it changes nothing.
        assert!(reg.observe_all(&[]).is_empty());
    }
}
