//! Unified, **persistent** tool-activity store — the backing feed of the Tool
//! Activity tab.
//!
//! Grew out of the V9-01 in-memory graph-call ring (`graph/activity.rs`): one
//! process-wide, newest-first history of tool calls, covering every feed —
//! `graph_*`/`context_*` calls (recorded by `graph::mcp::dispatch_recorded`
//! and friends), completed `offload_task` runs (recorded by
//! `offload::service`), Code Audit runs, and proxied MCP tool calls
//! (recorded by `offload::mcp_host::McpHost::call_recorded`) — with the
//! actual request/response payloads captured (truncated) so the UI can show
//! them in a detail popup.
//!
//! Entries survive an app restart: the ring is mirrored to a JSONL file next
//! to the executable (`<exe-dir>/tool-activity.jsonl`, the same portable
//! location as `settings.json`). Each `record` appends one line; `delete`/
//! `clear` rewrite the file; the load path compacts an over-long file back to
//! the retention caps. Payloads are size-capped at record time so a single
//! line stays modest and the file stays bounded by the caps.
//!
//! Retention is **per lane**, not global: graph/context calls are chatty
//! (every `graph_*` call, plus read-advisor reminders and auto-check
//! injections), while `offload_task` runs are rare and comparatively
//! valuable. A single shared cap would let a graph-heavy session silently
//! evict every offload row — the exact crowd-out the pre-V0.40.1 split
//! rings (graph 200 / offload 50 per backend) existed to prevent — so each
//! lane keeps its own newest-N window instead.
//!
//! A lane is a kind, except for `injection_flag`, where it is one **screen**
//! (#48, finding H-9 — see [`Lane`]). The invariant either way is the same one,
//! stated once: **no row source's volume may cost another source its history.**
//!
//! Process notes: when the app is running, the cloud-Claude path executes
//! in-process (loopback warm path) and the local offload worker also runs
//! in-process, so both land in this one store. In the app-not-running
//! fallback (and on a transient loopback failure while the app IS up) the
//! same code runs in the `cimp --offload-mcp` child, which appends to the
//! same file — those entries become visible in the app on the next launch.
//! Because another process can append lines this process's ring has never
//! seen, every rewrite that is *not* an explicit wipe (delete-one,
//! compaction, load-repair) first re-merges unknown on-disk entries into the
//! ring so it can't clobber them; `clear` intentionally skips the merge —
//! wiping the history is the point. A child appending in the instant between
//! that merge-read and the atomic rename can still lose that one entry;
//! that residual race is accepted for a best-effort activity log.
//!
//! Blocking note: `record`/`delete`/`clear` perform synchronous file I/O
//! under the store lock. Callers on async runtime threads must not call them
//! directly — use [`record_bg`] (recorders) or wrap in `spawn_blocking`
//! (IPC command handlers) so a slow disk / AV scan never stalls a tokio
//! worker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::offload::outbound::Screen;

/// Per-kind retention: how many entries of each kind to keep (in memory and
/// after compaction). `"offload"` gets its own window; every other kind
/// (currently just `"graph"`) uses the graph cap.
const GRAPH_CAP: usize = 400;
const OFFLOAD_CAP: usize = 100;
/// V23: Code Audit scan runs are rare (user-triggered) and comparatively
/// valuable — its own window so a graph-heavy session can't evict them.
const AUDIT_CAP: usize = 100;
/// Proxied MCP tool calls (`<server>__<tool>` through the warm host) sit
/// between the chatty graph calls and the rare offload runs in volume — their
/// own window so neither feed crowds the other out.
const MCP_CAP: usize = 200;
/// Offload **server process** lifecycle rows — spawned / ready / stopped /
/// failed, one per transition per backend (see
/// [`ActivityKind::OffloadServer`]).
///
/// Its own window rather than a share of `OFFLOAD_CAP`, and the reason is H-9's
/// in miniature: a backend that crashes is *also* a backend that runs no tasks,
/// so the rows explaining a bad session are the rare ones, and the busy period
/// either side of it is what would evict them. Sized for depth for the same
/// reason the forensic screens are — a healthy app run emits ~3 rows per local
/// backend (start, ready, stop at exit), so 200 is on the order of dozens of
/// app runs, while a crash-restart loop still cannot reach back and delete the
/// task history that shows what the server was doing when it died.
const OFFLOAD_SERVER_CAP: usize = 200;
/// V37 contract C6: MCP-server **health** transitions — a server confirmed
/// unhealthy, a server recovered, or a server that was enabled but could not be
/// connected at all.
///
/// Its own window for the reason `offload_server` has one, one layer up: a
/// server that is down serves no calls, so its `mcp` rows stop exactly when its
/// health rows start, and the busy servers either side of it are what would
/// evict the rows explaining the quiet one. Sized like `offload_server` (200)
/// because the volumes match — steady states mint nothing at all (the state
/// machine only writes on a transition), so a healthy app run emits zero rows
/// and a flapping endpoint emits two per cycle.
const MCP_HEALTH_CAP: usize = 200;
/// V33 Phase A: OS-sandbox rows — an unsandboxed degradation (with its distinct
/// `off (user choice)` / `unavailable` reason) or a grant/mapping event.
///
/// Its own window, chosen on purpose per #51 rather than left on the graph
/// fallback, and for the same reason `offload_server` has one: these rows are
/// rare and explanatory, while the feed around them is chatty. The row that
/// says "this tool call ran OUTSIDE the sandbox, because a prerequisite was
/// missing" is exactly the one a graph-heavy session would evict — and it is
/// the row decision 5 exists to guarantee. Small (60) because
/// `sandbox::record_skip` deduplicates by reason per session: a whole run of
/// unsandboxed spawns produces one row, not thousands.
const SANDBOX_CAP: usize = 60;
/// V38 Phase A: tool-plugin discovery rows — a manifest that failed to load, an
/// identity conflict, and the per-scan summary (see `crate::plugins::events`).
///
/// Its own window, chosen on purpose per #51 rather than left on the graph
/// fallback. 100 because the volume is *structurally* small and bursty: rows are
/// minted only at startup and on a manual Rescan — one per rejected file, plus
/// one summary — so 100 is many app runs' worth for a healthy folder, while a
/// user repeatedly editing a broken manifest and re-scanning (which is exactly
/// when these rows are being read) still cannot push the earlier attempts out
/// inside one sitting. Deliberately larger than [`SANDBOX_CAP`], which dedupes
/// per session and so needs no depth, and far smaller than the per-call lanes,
/// which this is not.
const PLUGIN_CAP: usize = 100;
/// V39 Phase B (locked decision 14): cross-harness **delegation** transitions —
/// one row per `start` / `done` / `refused` / `timeout` / `cancelled` /
/// `takeover` / `worker_exited` / `role_moved`.
///
/// Its own window, chosen on purpose rather than left on the graph fallback,
/// and for the reason `offload_server` has one: a delegation row is the durable
/// record of **who asked whom to do what** — the only place the attribution
/// survives (decision 2a keeps it out of the worker's transcript entirely, and
/// the on-tab banner dies with the flight). Those rows are rare and
/// explanatory while the feed around them is chatty, so the graph lane would
/// evict exactly the history a user goes looking for.
///
/// 100, matching `OFFLOAD_CAP`, because the volumes match by construction: a
/// worker is single-slot (decision 9), so delegations are serialized per tab
/// and a run mints at most two rows (`start` + one terminal transition). A
/// refusal loop is bounded the same way — it cannot outrun the model calling
/// the tool.
const DELEGATION_CAP: usize = 100;
/// #153: settings facts a user cannot see in their own settings file — today,
/// the project-overlay keys a save silently drops.
///
/// Its own window rather than the graph fallback (#51's rule), for the reason
/// `plugin` has one: the row is about a DEFINITION the user wrote, not about
/// anything that ran, and its whole job is to survive long enough to be read
/// the next time they wonder why a key in `.cimp/config.json` does nothing.
///
/// 30, the smallest window in the table, because the volume is structurally
/// tiny AND self-extinguishing: a row is minted only by a save that actually
/// drops something, and that same save rewrites the file without those keys —
/// so one hand-edit produces one row and then stops producing them. The only
/// way to reach 30 is thirty separate edits, at which point the older ones are
/// genuinely stale.
const SETTINGS_CAP: usize = 30;
/// V32: security denials (SSRF screen, fetch budgets, canary hits, taint-latch
/// refusals, quarantines, detection flags). Retained **per screen**, not per
/// kind — this is the window ONE screen's rows get.
///
/// A single `injection_flag` window (200, evicted oldest-first) was #48 finding
/// H-9: `MemoryQuarantine` writes one row per `context_note` — no latch, no
/// budget, no claim bit — so 200 notes carrying an `AKIA…`-shaped literal
/// evicted the `Canary` and `LatchBeacon` rows that were the only record of the
/// exfiltration that got through. Pinning the forensic screens would not have
/// closed it: `MemoryQuarantine` is *in* the forensic set and is also the flood
/// vector, so the same attack would have run inside the protected lane. What
/// closes it is that no screen shares a window with any other — see [`Lane`].
///
/// Sized for depth rather than for the old aggregate: 64 rows is more history
/// than any screen could rely on under the old shared 200 (then ~11 screens, ~18
/// each; `Screen::ALL` has since grown), and the rare forensic screens — canary hits, beacons, user latch
/// overrides — produce single digits in a whole session.
const INJECTION_FLAG_SCREEN_CAP: usize = 64;
/// How many lanes the `injection_flag` kind can hold: one per screen this
/// build declares, plus the single shared lane for sources it does not
/// recognize ([`UNKNOWN_SCREEN_LANE`]). Derived from `Screen::ALL`, so a new
/// screen adds a lane by existing.
const INJECTION_FLAG_LANES: usize = Screen::ALL.len() + 1;
/// The aggregate ceiling on `injection_flag` rows. Not a cap anything enforces
/// directly — it is the sum of the lane caps, which is what makes the feed
/// bounded even though no single counter bounds it.
const INJECTION_FLAG_TOTAL_CAP: usize = INJECTION_FLAG_SCREEN_CAP * INJECTION_FLAG_LANES;
/// Payload caps (chars) applied at record time — a request is typically a
/// small JSON args object or an offload instruction; responses can be large
/// tool output. Anything past the cap is cut with an explicit marker.
const REQUEST_CAP_CHARS: usize = 16_000;
const RESPONSE_CAP_CHARS: usize = 24_000;
/// On-disk mirror, next to the executable (same location as `settings.json`).
const FILE_NAME: &str = "tool-activity.jsonl";
/// Every row the store can hold with every lane full — the ring's size, and
/// the floor under [`FILE_COMPACT_LINES`].
///
/// **Derived from [`KINDS`]** (R23), not hand-summed. It used to be a chain of
/// ten named terms, and a kind added without one made the ring smaller than the
/// lanes it is supposed to hold — silently, because this sum is the only thing
/// in the module that knows how big "everything" is, and nothing checks a sum
/// against a table it was written beside. Now the table is the sum.
const TOTAL_CAPACITY: usize = total_capacity();

/// The fold behind [`TOTAL_CAPACITY`]. A `const fn` because the compaction
/// headroom is a compile-time assertion (see [`FILE_COMPACT_LINES`]), and a
/// runtime sum could not be one.
const fn total_capacity() -> usize {
    let mut sum = 0;
    let mut i = 0;
    while i < KINDS.len() {
        sum += KINDS[i].retention.ceiling();
        i += 1;
    }
    sum
}
/// Appends between compactions once the ring is full. Compaction rewrites the
/// whole file (and re-reads it first, to merge a child's lines), so this is the
/// amount of cheap appending bought per expensive rewrite.
const FILE_COMPACT_SLACK: usize = 500;
/// Compact (rewrite) once the appended file holds this many lines — bounds
/// file growth between loads without rewriting on every record.
///
/// **Derived**, not chosen (#48, H-9): a rewrite resets `file_lines` to
/// `ring.len()`, so a constant at or below [`TOTAL_CAPACITY`] means a saturated
/// store rewrites the entire file on **every** record. The old literal `1000`
/// was exactly the then-total, i.e. one screen-flood away from that cliff;
/// tying the two together is what keeps adding a lane from silently moving the
/// store onto it. `compaction_leaves_room_to_append` pins the relation.
const FILE_COMPACT_LINES: usize = TOTAL_CAPACITY + FILE_COMPACT_SLACK;

/// The headroom above, enforced at **compile time**: a compaction rewrite
/// resets `file_lines` to at most [`TOTAL_CAPACITY`], so the trigger must sit
/// STRICTLY above it — at or below, a saturated store rewrites the whole file
/// on every single record. `compaction_leaves_room_to_append` pins the same
/// relation at runtime, but only for whoever runs the test; this one refuses
/// to build.
///
/// Stated against `TOTAL_CAPACITY` rather than against
/// `TOTAL_CAPACITY + FILE_COMPACT_SLACK` on purpose: the latter is the *defining*
/// expression of [`FILE_COMPACT_LINES`], so asserting it was a tautology that
/// could not fire — including in the one case that matters, `FILE_COMPACT_SLACK`
/// being edited to 0 (survey defect D3).
const _: () = assert!(FILE_COMPACT_LINES > TOTAL_CAPACITY);

/// How one [`ActivityKind`]'s rows are retained — the cell that used to be a
/// `*_CAP` arm in [`kind_cap`]'s string if-chain and a term in the hand-summed
/// [`TOTAL_CAPACITY`], in the row itself.
#[derive(Clone, Copy)]
enum Retention {
    /// ONE window for the whole kind: the cap [`kind_cap`] answers with, and
    /// the kind's whole contribution to [`TOTAL_CAPACITY`].
    PerKind(usize),
    /// **No kind cap at all** — this kind's rows are retained PER SOURCE LANE,
    /// so a number here would be a cap nothing enforces.
    ///
    /// `injection_flag` is the one kind that carries it (#48, H-9): its rows
    /// share a kind but not a source, and the sources have wildly different
    /// volumes, so `MemoryQuarantine`'s flood could delete the `Canary` rows
    /// that were the only record of what got through. [`Lane`] gives each
    /// [`Screen`] its own window of [`INJECTION_FLAG_SCREEN_CAP`], and
    /// [`Lane::cap`] answers for such a row before [`kind_cap`] is ever asked.
    ///
    /// The value is therefore not a cap but the **aggregate ceiling** — the sum
    /// of the lane caps — which is what this kind is worth to
    /// [`TOTAL_CAPACITY`], and which is the only number about it the ring needs.
    PerSourceLanes(usize),
}

impl Retention {
    /// The most rows this kind can hold: one window, or all its lanes full.
    const fn ceiling(self) -> usize {
        match self {
            Retention::PerKind(cap) => cap,
            Retention::PerSourceLanes(total) => total,
        }
    }
}

/// One [`ActivityKind`]'s row in [`KINDS`]: the variant, its wire string, and
/// how its rows are retained.
///
/// R23 (V42): those three used to be four parallel structures — an enum arm, an
/// `as_str` arm, a `*_CAP` const reached only through a chain of STRING
/// comparisons, and a term in a hand-written `TOTAL_CAPACITY` sum. The sum was
/// the dangerous one: a kind added without a term in it made the ring smaller
/// than its own lanes, silently, because nothing else knows how big
/// "everything" is. Both are derived from this table now.
///
/// The named `*_CAP` consts above stay exactly where they are: each carries the
/// reasoning for its number, they are what the retention tests count against,
/// and one that no row references becomes an unused-constant WARNING — which is
/// the drift signal a bare literal in the table would have thrown away.
struct KindRow {
    /// The variant this row is about. Read by the const assertion below, which
    /// is what lets [`ActivityKind::as_str`] index [`KINDS`] by discriminant.
    kind: ActivityKind,
    /// The serialized form — a JSONL/IPC wire value, so a rename is a wire
    /// change and the feeds' kind filters are written against it.
    key: &'static str,
    retention: Retention,
}

/// Declare [`ActivityKind`], its wire strings and the [`KINDS`] table from one
/// list, so no two of the three can drift — the same shape `declare_screens!`
/// uses one module over, and for the same reason.
///
/// A variant added without a row is a macro PARSE error, and so is a row with a
/// cell left out: picking a retention lane on purpose (#51) is a decision the
/// compiler now insists on rather than a follow-up. That is the property the
/// four hand-kept structures never had — the if-chain compared STRINGS, so a
/// kind whose cap was never added to it simply fell through to the graph window
/// and was evicted by ordinary graph traffic.
macro_rules! declare_activity_kinds {
    (
        $(#[$enum_attr:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_attr:meta])*
                $variant:ident {
                    key: $key:literal,
                    retention: $retention:expr $(,)?
                }
            ),+ $(,)?
        }
    ) => {
        $(#[$enum_attr])*
        pub enum $name {
            $( $(#[$variant_attr])* $variant, )+
        }

        /// Every kind, in declaration order, with its wire string and its
        /// retention cell. Emitted from the variant list by
        /// [`declare_activity_kinds`], so it covers the enum exactly.
        const KINDS: &[KindRow] = &[ $(
            KindRow {
                kind: $name::$variant,
                key: $key,
                retention: $retention,
            },
        )+ ];

        impl $name {
            /// The serialized form — the row's `kind` column.
            ///
            /// Indexes [`KINDS`] by discriminant; the const assertion below is
            /// what says row *i* is variant *i*.
            pub const fn as_str(self) -> &'static str {
                KINDS[self as usize].key
            }

            /// The inverse of [`as_str`](Self::as_str): which kind a row that is
            /// already on disk belongs to.
            ///
            /// `None` means "not a kind this build declares" — a row written by
            /// a newer version, or under a wire value since retired. Readers
            /// that classify a row by its lane (see [`RowStatus::classify`])
            /// must be able to ask, and a `match` on the ENUM is what makes a
            /// new kind's classification a compile-time decision rather than a
            /// string comparison nobody added.
            pub fn from_wire(kind: &str) -> Option<$name> {
                match kind {
                    $( $key => Some($name::$variant), )+
                    _ => None,
                }
            }
        }
    };
}

declare_activity_kinds! {
/// The feed kind an activity belongs to. Kept as a closed enum at every
/// recording site (see [`ActivityEntry::new`]) so a new recorder can't typo a
/// kind string that would compile fine and silently vanish from the
/// kind-filtered feeds; the serialized form stays a plain string for
/// JSONL/IPC compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    /// A `graph_*` / `context_*` / `run_check` tool call (including the
    /// backend-internal read-advisor and auto-check recorders).
    Graph {
        key: "graph",
        // Also the window an UNKNOWN kind shares — see `kind_cap`.
        retention: Retention::PerKind(GRAPH_CAP),
    },
    /// One completed `offload_task` run.
    Offload {
        key: "offload",
        retention: Retention::PerKind(OFFLOAD_CAP),
    },
    /// V23: one completed audit tool run (a scanned tool within a Code Audit
    /// scan, source `"audit"`), or a whole `security_audit`/`quality_audit`
    /// agent call (roll-up row, source = the consumer — see
    /// `audit::mcp::run_audit`).
    Audit {
        key: "audit",
        retention: Retention::PerKind(AUDIT_CAP),
    },
    /// One proxied MCP tool call (`<server>__<tool>`) through the warm
    /// [`McpHost`](crate::offload::mcp_host::McpHost) — from Claude/OpenCode
    /// via the loopback `/mcp/call` route or from the offload worker's
    /// in-process router (recorded by `McpHost::call_recorded`).
    Mcp {
        key: "mcp",
        retention: Retention::PerKind(MCP_CAP),
    },
    /// V37 contract C6: one MCP-server **health** transition — a server the
    /// periodic checker confirmed unhealthy (N consecutive failed probes), a
    /// server that came back, or an enabled server `reconcile` could not connect
    /// at all.
    ///
    /// Distinct from [`ActivityKind::Mcp`], which is one *call* a server served,
    /// exactly as [`ActivityKind::OffloadServer`] is distinct from
    /// [`ActivityKind::Offload`] — and for the same reason: a server that is
    /// down serves no calls, so the rows explaining it cannot live in the
    /// window its traffic fills.
    ///
    /// Which transition it was lives in `tool`, why in `target`, and which
    /// producer saw it in `source`. Steady states mint nothing: only a
    /// transition writes, so an idle-but-healthy pool is silent rather than a
    /// heartbeat feed.
    McpHealth {
        key: "mcp_health",
        retention: Retention::PerKind(MCP_HEALTH_CAP),
    },
    /// One offload **server process** lifecycle transition: a local backend
    /// spawned, became healthy, was stopped, or failed to start. Recorded by
    /// [`offload::supervisor::lifecycle_record`](crate::offload::supervisor).
    ///
    /// Distinct from [`ActivityKind::Offload`], which is one *task* the server
    /// ran. Before this, the only trace of a server dying was the Settings log
    /// ring buffer — which `start_backend` clears on every (re)start, so a
    /// crash-restart erased its own evidence — plus an `offload-state` event
    /// nothing persisted. `ok` is the transition's outcome, and a `stop` is
    /// `true`: an intentional shutdown is not a failure. Which transition it
    /// was lives in `tool`, and *why* in `target`.
    OffloadServer {
        key: "offload_server",
        retention: Retention::PerKind(OFFLOAD_SERVER_CAP),
    },
    /// V32: one injection-containment denial — an SSRF-screened URL, an
    /// exhausted per-scope fetch budget, a canary hit, or a taint-latch
    /// refusal. Recorded by
    /// [`offload::outbound::record_flag`](crate::offload::outbound::record_flag);
    /// which screen fired is carried in the row's `source` field and (with the
    /// full detail) in its request payload.
    InjectionFlag {
        key: "injection_flag",
        // The ONE kind with no kind cap: retained per SOURCE lane, one window
        // per `Screen` (#48, H-9). See `Retention::PerSourceLanes` and `Lane`.
        retention: Retention::PerSourceLanes(INJECTION_FLAG_TOTAL_CAP),
    },
    /// V33 Phase A: one OS-sandbox fact — a child that ran UNSANDBOXED and why
    /// (the distinct `off (user choice)` / `unavailable` states of locked
    /// decision 17), or a grant/drive-mapping event. Recorded by
    /// [`sandbox::record_skip`](crate::sandbox::record_skip) and
    /// [`sandbox::record_event`](crate::sandbox::record_event).
    Sandbox {
        key: "sandbox",
        retention: Retention::PerKind(SANDBOX_CAP),
    },
    /// V38 Phase A: one tool-plugin discovery fact — a manifest that failed to
    /// load (with its reason), an identity conflict naming both offending
    /// files, or the per-scan summary. Recorded by
    /// [`plugins::events::record_scan`](crate::plugins::events::record_scan).
    ///
    /// Its own kind (decision 12) rather than a source under an existing one:
    /// these rows are about DEFINITIONS, not about anything that ran, and the
    /// settings pane pairs each of them with an error state — the two surfaces
    /// exist so a rejected plugin is visible both where it happened and where
    /// it gets fixed.
    Plugin {
        key: "plugin",
        retention: Retention::PerKind(PLUGIN_CAP),
    },
    /// V39 Phase B (locked decision 14): one cross-harness **delegation**
    /// transition — a tab was asked to drive another tab, and what came of it.
    ///
    /// Which transition it was lives in `tool` (`start` / `done` / `refused` /
    /// `timeout` / `cancelled` / `takeover` / `worker_exited` / `role_moved`),
    /// the worker tab (plus the reason, on a refusal) in `target`, the driver
    /// HARNESS in `source` and the driver TAB in the attribution — so a reader
    /// can answer "who asked" without the worker's own transcript containing
    /// the answer, which by decision 2a it deliberately does not.
    ///
    /// **A facade run mints two rows by design, not one**, and they are not
    /// duplicates: the driver side already writes an [`ActivityKind::Offload`]
    /// row for every completed `offload_task` regardless of backend kind (its
    /// `source` is the backend NAME, so it reads `lan-worker-2` and the facade
    /// holds here too), while these rows are the worker side. Same split as
    /// `offload` vs `offload_server`: the task, versus what carried it.
    Delegation {
        key: "delegation",
        retention: Retention::PerKind(DELEGATION_CAP),
    },
    /// #153: one settings fact the settings FILE cannot report about itself —
    /// today, the project-overlay keys a save dropped (`source` `"overlay"`,
    /// `tool` `"stray_keys"`, the dotted names in `target`). Recorded by
    /// [`settings::persistence`](crate::settings::persistence).
    ///
    /// Its own kind rather than a source under `plugin` (decision 12's rule,
    /// applied where it points): a `plugin` row is about a manifest FILE the
    /// user can open and fix in the plugins folder, and this is about a key in
    /// their own project config. Folding them together would put two different
    /// "go edit this" instructions behind one filter.
    ///
    /// `ok` is `true`: nothing failed. cImp did exactly what it says it does
    /// with a key it will not honour — the row exists so that doing it is not
    /// also SILENT, which is the whole of the defect.
    Settings {
        key: "settings",
        retention: Retention::PerKind(SETTINGS_CAP),
    },
}
}

/// [`ActivityKind::as_str`] indexes [`KINDS`] by discriminant, and this is what
/// says it may: row *i* is variant *i*, checked at COMPILE time rather than
/// trusted.
///
/// It holds by construction — [`declare_activity_kinds`] emits the enum and the
/// table from one list, in one order — but every derivation in this module rests
/// on it, so it is asserted rather than assumed. Reading `kind` here is also
/// what keeps that cell honest: a row naming the wrong variant would otherwise
/// be a field nobody reads.
const _: () = {
    let mut i = 0;
    while i < KINDS.len() {
        assert!(
            KINDS[i].kind as usize == i,
            "`KINDS` must list every `ActivityKind` in enum-declaration order — \
             `as_str` indexes it by discriminant"
        );
        i += 1;
    }
};

/// The retention cap for a (serialized) kind. Unknown strings (a future kind
/// loaded from a newer file) share the graph window rather than erroring.
///
/// `injection_flag` answers the same way and for a different reason: it has no
/// kind cap at all ([`Retention::PerSourceLanes`]) because its rows are counted
/// per SOURCE (see [`Lane`]), and [`Lane::cap`] takes that branch before this
/// function is ever reached for one.
///
/// **Adding an event class? Pick a lane on purpose** (#51). The graph-window
/// fallback below exists for FORWARD COMPAT (a file written by a newer build
/// must still load here), not as a home: a kind left on it can be evicted by
/// ordinary graph traffic, which is exactly the failure H-9 closed for the
/// containment screens. A new kind recorded by *this* build gets its own cap
/// in [`KINDS`] — or a per-source lane split like `injection_flag`'s — as part
/// of being added, which the table now makes compulsory rather than a
/// follow-up.
fn kind_cap(kind: &str) -> usize {
    match KINDS.iter().find(|row| row.key == kind).map(|r| r.retention) {
        Some(Retention::PerKind(cap)) => cap,
        Some(Retention::PerSourceLanes(_)) | None => GRAPH_CAP,
    }
}

/// The lane an `injection_flag` row lands in when its `source` is not a screen
/// this build declares — a row written by a newer version, or under a wire
/// value since retired.
///
/// They share one bounded lane rather than getting one each: the set of foreign
/// strings is not knowable, and a lane per distinct string would make an
/// unrecognized file an unbounded growth channel. They still cannot evict a
/// known screen, which is the property that matters. Cannot collide with a real
/// wire value — every screen's is a lowercase snake-case identifier, pinned by
/// `the_unknown_lane_is_not_a_screen_name`.
const UNKNOWN_SCREEN_LANE: &str = "?unknown";

/// One retention window. Rows compete for eviction **only** against other rows
/// in their own lane, and every lane has its own cap, so the store's whole
/// retention contract is one sentence: *a row is evicted only by newer rows of
/// its own lane.*
///
/// For every kind but one, a lane IS the kind. For `injection_flag` a lane is
/// one [`Screen`] (#48, finding H-9): those rows all share a kind but not a
/// source, and the sources have wildly different volumes — `MemoryQuarantine`
/// fires once per `context_note`, `Canary` fires when an exfiltration is caught.
/// Sharing a window meant the chatty one deleted the rare one's evidence, which
/// an attacker can drive on purpose.
///
/// **A new screen is protected by construction.** The lane set comes from
/// `Screen::ALL`, which `declare_screens!` emits from the variant list, so a
/// variant added tomorrow has its own guaranteed window the moment it exists.
/// Sharing a window with another screen would take a deliberate edit here.
/// `Screen::Contamination` (finding F-3) is the first variant to arrive under
/// that guarantee: it was added to the enum and to nothing else, and it has its
/// own lane and its own row in the generated eviction matrix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Lane {
    /// The row's kind, verbatim (unknown kinds keep their own string and share
    /// the graph cap, as they always have).
    kind: String,
    /// The screen, for `injection_flag` rows only: a `Screen::as_str()` when the
    /// source is recognized, [`UNKNOWN_SCREEN_LANE`] when it is not, `None` for
    /// every other kind.
    screen: Option<&'static str>,
}

impl Lane {
    /// Which lane an entry belongs to. Total and cheap — this runs once per row
    /// per eviction pass, on the write path.
    fn of(entry: &ActivityEntry) -> Self {
        let screen = if entry.kind == ActivityKind::InjectionFlag.as_str() {
            // `source` carries the screen for every injection_flag writer
            // (`outbound::record_flag` and the C3 updater's `record_row` both
            // stamp `Screen::as_str()`); anything else came off disk.
            Some(Screen::from_wire(&entry.source).map_or(UNKNOWN_SCREEN_LANE, |s| s.as_str()))
        } else {
            None
        };
        Self {
            kind: entry.kind.clone(),
            screen,
        }
    }

    /// How many rows this lane retains.
    fn cap(&self) -> usize {
        match self.screen {
            Some(_) => INJECTION_FLAG_SCREEN_CAP,
            None => kind_cap(&self.kind),
        }
    }
}

/// Who a row is attributed to — the "which tab is doing what" column (#51).
///
/// Four states, and **collapsing any two of them is a bug**:
///
/// * [`Tab`](Self::Tab) — a configured AI tab. The only state that may render
///   as a tab.
/// * [`Unrecognized`](Self::Unrecognized) — a non-empty id naming no configured
///   tab. `loopback::tab_identity`'s `Unknown`: it "creates no row and gates
///   nothing". Rendering it as a tab would attribute activity to a tab that
///   does not exist, inside the view whose job is attribution.
/// * [`Headless`](Self::Headless) — positively no tab. Covers the documented
///   first-class headless consumers (`claude -p`, cron), worker tasks, and
///   cImp's own internal work (the read advisor, auto-check, the C3 updater).
///   This is a fact about the caller, not missing data — which is exactly why
///   it must stay distinct from `Unattributed` below.
/// * [`Unattributed`](Self::Unattributed) — this writer does not know. Also
///   what a row written before #51 deserializes to, which is why it is
///   `Default`: an old row must not claim to be `Headless`, because "nobody was
///   asking on a tab" and "we weren't recording it yet" are different facts and
///   only one of them is evidence.
///
/// Wire form is a plain externally-tagged enum, so `Tab`/`Unrecognized` cost one
/// short string and the two unit variants cost a word — [`ActivityEntry`] is
/// polled every couple of seconds and has to stay light.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    /// The writer does not know, or the row predates the column.
    #[default]
    Unattributed,
    /// Positively no tab: a headless consumer.
    Headless,
    /// A configured AI tab id, or an id that reached the recorder as
    /// cImp-authored argv (`--tab`), which a request body cannot forge.
    Tab(String),
    /// A non-empty id that names no configured tab.
    Unrecognized(String),
}

// `id`/`is_tab` have no Rust caller: the Events tab narrows client-side, so the
// filter-by-tab rule lives in `src/lib/activity.ts` (see `activity_list` in
// `ipc::commands` for why the server-side filter was removed). They stay,
// unit-tested, because this enum is where the four states are DEFINED and
// `is_tab`'s exclusion of `Unrecognized` is the one thing a reader of that
// definition must not have to infer. Unlike the filter that was removed, a
// one-line `matches!` over the variants is not a second matching pipeline that
// can drift out of step — it is the definition restating itself.
#[allow(dead_code)]
impl Attribution {
    /// Attribution for a recorder running **inside a cImp-spawned child**,
    /// from that child's own `--tab` argv.
    ///
    /// `Some` ⇒ [`Tab`](Self::Tab), with no configured-tab check, and that is
    /// correct rather than lax: `--tab <id>` is "composed entirely by cImp at
    /// spawn on both consumers' paths and nothing in a request body can reach
    /// it" (`graph::mcp`, test-pinned on both consumers). If cImp spawned this
    /// child for that tab, the tab is real; there is no forgeable path into
    /// this value, so there is nothing for a registry lookup to catch.
    ///
    /// `None` ⇒ [`Headless`](Self::Headless), not `Unattributed`: a child with
    /// no `--tab` was not spawned by cImp at all — the documented first-class
    /// headless consumers (`claude -p`, cron) — so "no tab" is a fact here, not
    /// an absence of information.
    ///
    /// **Not for app-side recorders.** A tab id that arrived over the loopback
    /// route came from a request body, which a caller can invent; those must
    /// classify through `loopback::tab_identity` so an id naming no configured
    /// tab becomes [`Unrecognized`](Self::Unrecognized). Same field, different
    /// provenance — this constructor exists so the two cannot be confused at a
    /// call site.
    pub fn from_child_argv(tab: Option<&str>) -> Self {
        match tab.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => Attribution::Tab(t.to_string()),
            None => Attribution::Headless,
        }
    }

    /// The id to display, for the two states that carry one.
    pub fn id(&self) -> Option<&str> {
        match self {
            Attribution::Tab(t) | Attribution::Unrecognized(t) => Some(t.as_str()),
            _ => None,
        }
    }

    /// Whether this row is attributable to a REAL tab — the predicate a
    /// "filter by tab" must use. Deliberately false for
    /// [`Unrecognized`](Self::Unrecognized): filtering by a tab id must never
    /// surface a row that merely quoted that id.
    pub fn is_tab(&self) -> bool {
        matches!(self, Attribution::Tab(_))
    }

}

// ── Row status (#48, M-24) ─────────────────────────────────────────────────
//
// **Why this is here and not in either feed component.** Both the Tool Activity
// tab and the Events tab render this store, and both collapsed every
// `injection_flag` row into one treatment: Tool Activity painted the whole kind
// chip danger-red ("the only kind with a tinted chip"), and Events mapped
// `ok ? 'flagged' : 'denied'`. So `unscreened`, the two detector screens,
// `memory_quarantine` and `latch_override` all arrived on screen as the same
// alarm — and `unscreened`, whose entire meaning is *"we did not look at all of
// it"*, read as *"we blocked something"*, which is the opposite of the truth.
// `latch_override` — a user GRANTING capability back — read as containment
// firing. One classifier, consumed by both feeds: the security vocabulary of
// this app must not differ between two tabs showing the same rows.
//
// **Why it is here and not in `src/lib/activity.ts` (V42).** It was a ~120-line
// pure function of one row, and every branch in it was a restatement of a rule
// that lives on this side: [`Screen::is_denial`], the `updater` source written
// outside `record_flag`, the transition verbs the lifecycle recorders mint,
// `SCREEN_DROP_SOURCE`. A restatement cannot be checked against the thing it
// restates — a screen added here simply fell through to the frontend's
// "unknown" branch — so the classifier moved to where those rules are: it now
// matches [`Screen`] exhaustively (a new screen must choose a word or the build
// fails) and `every_denial_screen_and_only_a_denial_screen_reads_as_denied`
// holds the vocabulary to `is_denial` itself. What stayed on the frontend is the
// part that is genuinely presentation: the tooltip sentences (`STATUS_TITLE`)
// and the chip's CSS.

/// Sources that are TELEMETRY CHANNELS rather than tool invocations:
/// `read_advisor` reports advisor reminders and full-file-`Read` bypasses,
/// `harness` reports contract/sub-agent drift. Both record `ok: false` to mean
/// "this signal fired", not "this call failed" — so painting them with the error
/// colour made the feed read as mostly-broken when nothing had broken (20 of 28
/// red rows on one machine were bypass canaries).
const CANARY_SOURCES: [&str; 2] = ["read_advisor", "harness"];

/// Declare [`RowStatus`], its wire words and [`RowStatus::ALL`] from one list,
/// so the word a row carries, the class a chip is drawn with and the set the
/// guards iterate cannot drift. Same shape as [`declare_activity_kinds`] above
/// and `declare_screens!` two modules over, for the same reason.
macro_rules! declare_row_statuses {
    (
        $(#[$enum_attr:meta])*
        pub enum $name:ident {
            $( $(#[$variant_attr:meta])* $variant:ident => $wire:literal ),+ $(,)?
        }
    ) => {
        $(#[$enum_attr])*
        pub enum $name {
            $( $(#[$variant_attr])* $variant, )+
        }

        impl $name {
            /// Every status, in declaration order — derived from the variant
            /// list, not written beside it. Read by the guards that hold this
            /// vocabulary to the frontend's.
            #[cfg_attr(not(test), allow(dead_code))]
            pub const ALL: &'static [$name] = &[ $( $name::$variant, )+ ];

            /// The word itself: the serialized value, the chip's CSS class and
            /// the label the feeds render. One string, because a status the
            /// frontend has no word for cannot be drawn.
            pub const fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $wire, )+ }
            }
        }
    };
}

declare_row_statuses! {
/// What one feed row actually reports, as one word.
///
/// Three plain call outcomes:
/// * `ok` — the call worked.
/// * `failed` — the call failed.
/// * `signal` — a telemetry channel fired ([`CANARY_SOURCES`]); not a failure.
///
/// …and nine `injection_flag` outcomes, which are NOT interchangeable:
/// * `denied` — a screen stopped the call. The only one that means "we blocked
///   something", and the only one wearing danger. Reached by exactly the
///   screens [`Screen::is_denial`] answers `true` for, which is a test, not a
///   convention.
/// * `flagged` — a detector matched and the result was delivered anyway
///   (detection is surface-only, locked decision 5).
/// * `unscreened` — part of the result was never looked at. Nothing found,
///   nothing stopped: the absence of a verdict is not a verdict of absence.
/// * `held` — a memory write was stored and withheld pending human review.
/// * `engaged` — containment came ON for a tab (a native-web beacon; a
///   conversation becoming contaminated). Nothing was refused.
/// * `granted` — a user gave capability back (a latch override; a contamination
///   flag cleared). A release, not a block — which is exactly why it must not
///   share a treatment with one.
/// * `update` / `rejected` — the detection auto-updater acted, or refused a
///   bundle. Not a screen over a tool call at all.
/// * `recorded` — a containment row this build has no category for.
///   Deliberately NOT folded into `denied` or `flagged`: it must render as "we
///   do not have a word for this" rather than inherit a claim.
///
/// …and four `offload_server` outcomes. `stopped` and `down` are the pair that
/// must not merge, for the same reason `denied` and `granted` must not: cImp
/// killing a server on purpose and a server failing to come up are opposite
/// facts that both end with no process running, and only one of them is
/// something going wrong.
/// * `started` — the process was spawned. Says nothing about health yet.
/// * `ready` — it answered `/health`; the window and slot count were read.
/// * `stopped` — cImp stopped it deliberately (see the row's target for which
///   intent: user, restart, or app shutdown). Not a failure.
/// * `down` — it never came up, or it ended without cImp stopping it.
///
/// …and three `delegation` transitions that are not call outcomes at all, and
/// so cannot borrow one. Under the plain `ok`/`failed` fallthrough a `start`
/// row read "Call succeeded" before anything had happened, a `takeover` — the
/// user reclaiming their own tab — read "Call failed", and a `role_moved` row,
/// which is a configuration change, read as a call. Each is one fact and gets
/// one word (the `plugin` lane's rule, applied in the direction it points: a
/// kind whose outcomes really are two gets no synonyms; a kind whose rows are
/// not outcomes gets its own words rather than a borrowed claim).
///
/// …and one `settings` word, `dropped`: keys a save removed from a project
/// overlay. Minted `ok: true`, so the plain tail would render the one lane that
/// exists to break a silence as "Call succeeded".
///
/// The `plugin` lane adds no word on purpose: a definition either loaded or it
/// did not, and `ok`/`failed` say that exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// The call worked.
    Ok => "ok",
    /// The call failed.
    Failed => "failed",
    /// A telemetry channel fired — not a failure. See [`CANARY_SOURCES`].
    Signal => "signal",
    /// An offload server process was spawned. Says nothing about health yet.
    Started => "started",
    /// An offload server answered `/health`.
    Ready => "ready",
    /// cImp stopped an offload server deliberately. Not a failure.
    Stopped => "stopped",
    /// An offload server never came up, or ended without cImp stopping it.
    Down => "down",
    /// V39: a delegation started — cImp typed the request and began waiting.
    /// The row that says how it ended is the next one for that worker.
    Driving => "driving",
    /// V39: the user took the tab back mid-flight. Deliberate, and the worker
    /// kept running: never a failure.
    Takeover => "takeover",
    /// V39: the Manual role for a harness moved off this tab. Configuration,
    /// not traffic.
    Moved => "moved",
    /// #153: keys a settings save removed from a project overlay — either this
    /// build has no setting by that name, or the setting is machine scope.
    ///
    /// Its own word, and neither of the two it would otherwise take. The row is
    /// minted `ok: true` (nothing failed — cImp did exactly what it documents),
    /// so [`RowStatus::plain`] renders it `ok`: a green "Call succeeded" over
    /// the news that part of the user's config was discarded. `failed` claims
    /// the opposite falsehood, that cImp broke. What happened is that something
    /// the user wrote is NOT in effect, deliberately and by rule — and the row
    /// exists precisely because that used to be silent, so its word must not
    /// re-silence it.
    Dropped => "dropped",
    /// A screen stopped the call — this app's one "we blocked something".
    Denied => "denied",
    /// A detector matched and the result was delivered anyway.
    Flagged => "flagged",
    /// Part of the result was never looked at.
    Unscreened => "unscreened",
    /// A memory write was stored and withheld pending human review.
    Held => "held",
    /// Containment came ON for a tab. Nothing was refused.
    Engaged => "engaged",
    /// A user gave capability back. A release, not a block.
    Granted => "granted",
    /// The detection auto-updater acted on a bundle.
    Update => "update",
    /// The detection auto-updater REFUSED a bundle. Not a blocked call.
    Rejected => "rejected",
    /// A containment row this build has no category for.
    Recorded => "recorded",
    /// V33 Phase A: a child ran outside the OS sandbox. Distinct from `denied`
    /// (a model was refused) and from `failed` (the command itself broke) — the
    /// command ran fine, the boundary was absent.
    Unsandboxed => "unsandboxed",
    /// A sandboxed child failed with output MATCHING an access-denial
    /// signature.
    ///
    /// Its own word rather than `denied` or `failed`, because it is neither.
    /// `denied` is this app's one "we stopped it" — filled red, a certainty cImp
    /// does not have here: it cannot observe the OS's ACL decision, only the
    /// exit code and stderr the child chose to print, so the backend words this
    /// row as a labeled heuristic and the chip must not out-claim it. `failed`
    /// is wrong the other way: the call itself returned normally (a nonzero exit
    /// is still output the model receives), and reading a boundary hit as an
    /// ordinary broken command is exactly the confusion this row exists to end.
    Boundary => "boundary",
    /// V37 C6: an MCP server was confirmed unhealthy (the flap guard tripped),
    /// or an enabled one could not be connected at all.
    ///
    /// Its own word rather than `down`, whose tooltip is written about an
    /// offload SERVER PROCESS cImp owns — an MCP server is somebody else's
    /// process (or somebody else's URL), cImp neither started nor stopped it,
    /// and reading one row's vocabulary onto the other would promise a lifecycle
    /// this app does not have.
    Unhealthy => "unhealthy",
    /// V37 C6: an MCP server answered again after having been unhealthy. The
    /// row contract C6 guarantees follows every error row about a server that
    /// came back, so an error is never the lane's last word.
    Recovered => "recovered",
    /// V37 C9: an external server's tool was WITHHELD from every consumer's
    /// advertised surface because description screening flagged its name or
    /// description. It lives in the `mcp` lane beside call rows, but no call
    /// ever happened.
    ///
    /// Its own word, and neither of the two it kept falling into: `failed`
    /// claims a call that was never made, and `flagged` — whose whole promise is
    /// "nothing was blocked" — reports the one place in cImp where detection
    /// really does REMOVE something as a delivery.
    Withheld => "withheld",
}
}

impl serde::Serialize for RowStatus {
    /// One word, from [`RowStatus::as_str`] — the same string the chip is
    /// classed with. There is deliberately no `Deserialize`: the stored word is
    /// never read back (see [`ActivityEntry::status`]).
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl RowStatus {
    /// The value `serde` leaves in [`ActivityEntry::status`] while parsing, and
    /// **never a word anything renders**: the field is `skip_deserializing`, and
    /// [`parse_line`] — the one function that turns a JSONL line into a record —
    /// re-derives the real word immediately. It exists only because
    /// `skip_deserializing` needs something to put there.
    ///
    /// `Recorded` rather than `Ok` or `Failed` on purpose: if a future reader
    /// ever parses a row without re-deriving, it renders as "this build has no
    /// word for it" — the one word in the vocabulary that claims nothing —
    /// instead of asserting that a call succeeded or failed.
    fn unclassified() -> RowStatus {
        RowStatus::Recorded
    }

    /// Classify one row.
    ///
    /// `ok` is read as the denial predicate ONLY for the screens that follow it.
    /// [`Screen::is_denial`] is the rule and
    /// [`record_flag`](crate::offload::outbound::record_flag) publishes it as
    /// `ok: false` — but `updater` rows are the one source written outside
    /// `record_flag`, where `ok` is the bundle OUTCOME (`rejected ⇒ false`).
    /// Reading `!ok` as "denied" there would report a refused rules bundle as a
    /// blocked tool call, which is the same collapse this function exists to
    /// undo, so `updater` is matched before `ok` is consulted at all.
    ///
    /// Every lane whose rows are TRANSITIONS is keyed on `tool` (the verb),
    /// never on `ok` alone: `ok` is true for both a healthy start and a
    /// deliberate stop, so reading it by itself would render "the server is
    /// gone" and "the server is up" as one word.
    pub fn classify(e: &ActivityEntry) -> RowStatus {
        match ActivityKind::from_wire(&e.kind) {
            Some(ActivityKind::OffloadServer) => match e.tool.as_str() {
                "start" => RowStatus::Started,
                "ready" => RowStatus::Ready,
                "stop" => RowStatus::Stopped,
                "fail" => RowStatus::Down,
                // A transition added later than this reader. `ok` is documented
                // as the transition's outcome, which is a claim we can still
                // make; the verb it belongs to is not.
                _ => {
                    if e.ok {
                        RowStatus::Ok
                    } else {
                        RowStatus::Down
                    }
                }
            },
            // `ok` here distinguishes a CHOSEN unsandboxed state (the switch is
            // off) from an unavailable one, so it cannot also carry the verb.
            // Locked decision 17 requires those two to stay visibly distinct,
            // which the row's `target` text spells out ("off (user choice)" /
            // "unavailable").
            Some(ActivityKind::Sandbox) => match e.tool.as_str() {
                "unsandboxed" => RowStatus::Unsandboxed,
                // A child ran INSIDE the boundary. Deliberately quiet: this is
                // the expected case, and the row exists to remove the empty-lane
                // ambiguity ("everything was sandboxed" vs "nothing ever ran"),
                // not to compete for attention.
                "sandboxed" => RowStatus::Ok,
                "denied" => RowStatus::Boundary,
                // `grant`, drive mappings, and anything added later: `ok` is
                // that event's outcome, which is a claim we can still make.
                _ => Self::plain(e),
            },
            Some(ActivityKind::McpHealth) => match e.tool.as_str() {
                "unhealthy" | "connect_failed" => RowStatus::Unhealthy,
                "healthy" => RowStatus::Recovered,
                _ => {
                    if e.ok {
                        RowStatus::Recovered
                    } else {
                        RowStatus::Unhealthy
                    }
                }
            },
            Some(ActivityKind::InjectionFlag) => Self::for_screen(e),
            // V39 locked decision 14. Only the rows that are NOT call outcomes
            // are named; `done`, `refused`, `timeout` and `worker_exited` fall
            // through, where `ok`/`failed` say exactly what happened and a
            // synonym would dilute the vocabulary.
            Some(ActivityKind::Delegation) => match e.tool.as_str() {
                "start" => RowStatus::Driving,
                "takeover" => RowStatus::Takeover,
                "role_moved" => RowStatus::Moved,
                _ => Self::plain(e),
            },
            // V37 C9, and BEFORE the plain fallthrough on purpose: these rows
            // are minted `ok: false` in the `mcp` lane, so without this a
            // withheld tool renders as "Call failed" — a claim about a call that
            // never happened. Keyed on the exact wire source, never on the kind
            // alone: an ordinary failed call on the same lane stays `failed`.
            Some(ActivityKind::Mcp) if e.source == crate::offload::mcp_host::SCREEN_DROP_SOURCE => {
                RowStatus::Withheld
            }
            // #153, and BEFORE the plain fallthrough for the same reason the arm
            // above it is: these rows are minted `ok: true`, so without this the
            // one lane whose whole purpose is to break a silence renders as
            // "Call succeeded". The whole kind takes the word — unlike the
            // verb-keyed lanes above, every row here reports the same fact.
            Some(ActivityKind::Settings) => RowStatus::Dropped,
            _ => Self::plain(e),
        }
    }

    /// The three plain call outcomes — the tail every lane without a verb of its
    /// own falls through to.
    fn plain(e: &ActivityEntry) -> RowStatus {
        if e.ok {
            RowStatus::Ok
        } else if CANARY_SOURCES.contains(&e.source.as_str()) {
            RowStatus::Signal
        } else {
            RowStatus::Failed
        }
    }

    /// One `injection_flag` row, by the [`Screen`] that wrote it.
    ///
    /// Exhaustive over the enum on purpose: a screen added to `declare_screens!`
    /// does not compile until it has picked a word, which is the whole reason
    /// this classification moved next to the screens. The four that read
    /// `denied` are exactly [`Screen::is_denial`]'s set — pinned by
    /// `every_denial_screen_and_only_a_denial_screen_reads_as_denied` rather
    /// than by the two lists happening to agree.
    fn for_screen(e: &ActivityEntry) -> RowStatus {
        let Some(screen) = Screen::from_wire(&e.source) else {
            // A screen a WRITER declares and this reader does not (a row from a
            // newer build, or a wire value since retired). `ok: false` still
            // carries `Screen::is_denial`, which is a claim we can make; a
            // delivered one gets the no-category word rather than a borrowed
            // one.
            return if e.ok {
                RowStatus::Recorded
            } else {
                RowStatus::Denied
            };
        };
        match screen {
            // Checked before `ok` is read as a denial: its `ok` is the bundle
            // outcome (see this function's sibling doc).
            Screen::Updater => {
                if e.ok {
                    RowStatus::Update
                } else {
                    RowStatus::Rejected
                }
            }
            Screen::Signature | Screen::Classifier => RowStatus::Flagged,
            Screen::Unscreened => RowStatus::Unscreened,
            Screen::MemoryQuarantine => RowStatus::Held,
            Screen::LatchBeacon | Screen::Contamination => RowStatus::Engaged,
            Screen::LatchOverride | Screen::ContaminationCleared => RowStatus::Granted,
            // Containment that WORKED: the child skipped a planted discovery
            // entry and reached the real instance, so nothing was refused and
            // nothing failed. It has no word of its own — `recorded` is what the
            // frontend classifier gave it too, by falling through — and giving
            // it one is a UI decision, not a refactoring one.
            Screen::DiscoverySkipped => RowStatus::Recorded,
            Screen::Ssrf | Screen::Budget | Screen::Canary | Screen::LatchRefusal => {
                RowStatus::Denied
            }
        }
    }
}

/// One recorded tool activity, WITHOUT payloads — the shape list consumers
/// (the Tool Activity feed poll, the Graph View pulse feed) receive every
/// couple of seconds, so it must stay light.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ActivityEntry {
    /// Stable id (unique across restarts) — what delete-by-id keys on.
    /// Derived from the record timestamp plus a randomly-seeded per-process
    /// sequence (see [`next_id`]); the load path additionally repairs any
    /// duplicate that slips through from another process.
    #[serde(default)]
    pub id: u64,
    /// Unix epoch millis when the call started.
    pub ts_ms: u64,
    /// Feed kind, the serialized form of [`ActivityKind`].
    #[serde(default = "default_kind")]
    pub kind: String,
    /// The project this row belongs to, as [`root_key`]. Lets a per-project
    /// consumer (the Graph View pulse feed) filter out other projects' activity.
    ///
    /// **Empty means "not attributable to a project" — a claim, not a gap**
    /// (#48 F-16). It covers a writer that could not derive one (an offload run
    /// with no session cwd) AND a row that is genuinely not about a project (the
    /// harness `contract_drift` report), and it is deliberately ONE sentinel: it
    /// is also what a row written before this column existed deserializes to, and
    /// such a row is honestly unknown. A future recorder that positively has "no
    /// project" as a *fact* must not reuse this — at that point `root` becomes an
    /// enum the way [`Attribution`] is, and this comment is the note saying so.
    ///
    /// Consumers must never silently HIDE a row for being rootless. The forensic
    /// screens cannot produce one
    /// (`offload::outbound::tests::every_forensic_screen_row_carries_a_project_root`),
    /// and the view rule for anything that slips through belongs beside the root
    /// filter in `src/lib/activity.ts`.
    pub root: String,
    /// Who issued it: `"claude"` / `"opencode"` / `"offload"` /
    /// `"read_advisor"` / `"auto_check"` for graph entries; the backend name
    /// for offload entries; `"audit"` for per-scanner audit rows and the
    /// consumer (`"claude"` / `"opencode"` / `"offload"`) for audit roll-up
    /// rows and MCP entries. For V32 `injection_flag` rows it names the SCREEN
    /// that fired (`"ssrf"` / `"budget"` / `"canary"` / `"latch_refusal"`) —
    /// which screen denied a call is the fact worth reading at a glance, and
    /// the issuing consumer is carried in that row's request payload instead.
    pub source: String,
    /// The tool name, e.g. `graph_find_symbol` or `offload_task`.
    pub tool: String,
    /// The primary argument (symbol / file / query / instruction headline).
    pub target: String,
    /// Response size in characters.
    pub chars: usize,
    /// Wall-clock duration in milliseconds.
    pub ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
    /// **What this row reports, as one word** — the chip both feeds render, and
    /// the reading of `ok` that `ok` alone cannot give (see [`RowStatus`]).
    ///
    /// **Derived, never input.** [`ActivityEntry::new`] classifies at the
    /// recording site, [`ActivityStore::record`] re-classifies whatever it is
    /// handed, and [`parse_line`] re-derives it for every line it reads back —
    /// so a row written by an older build renders in TODAY's vocabulary rather
    /// than in the one it was written under. The word is serialized (a JSONL
    /// line stays readable on its own) but `skip_deserializing` says the stored
    /// copy is never trusted: the classifier is the authority, on disk as on the
    /// wire.
    #[serde(skip_deserializing, default = "RowStatus::unclassified")]
    pub status: RowStatus,
    /// #51: which tab this row belongs to. See [`Attribution`] — an absent
    /// field (every row written before #51) reads as
    /// [`Attribution::Unattributed`], never as `Headless`.
    #[serde(default)]
    pub tab: Attribution,
    /// #51: the harness conversation the caller was in, when the writer knows
    /// it.
    ///
    /// **A separate field from `tab` on purpose** (#48 F-3): a tab outlives its
    /// conversations, so `tab` alone cannot answer "which conversation was
    /// this?", and a consumer joining a row to something conversation-shaped —
    /// a checkpoint, a transcript — needs an exact key rather than a guess by
    /// nearest wall clock. `None` for a worker task (no harness session), for a
    /// tab whose session the registry withholds, and for every pre-#51 row.
    #[serde(default)]
    pub session: Option<String>,
    /// V37 contract C7: which MCP server this row is about — the `mcp` call
    /// rows and every [`ActivityKind::McpHealth`] row. `None` for every other
    /// kind and for every row written before this column existed.
    ///
    /// **Never derived by splitting `tool` on `__`.** A server name or a raw
    /// tool name may itself contain `__`, so the split routes to the wrong (or
    /// to a nonexistent) server — `offload::mcp_host` has documented that hazard
    /// since V8-03 and routes by ownership instead. The writer knows the owner
    /// from the same routing fact that dispatched the call, and this column is
    /// that fact recorded, not re-guessed.
    #[serde(default)]
    pub server: Option<String>,
    /// V37 contract C7: the MCP category `server` belongs to — the FIRST
    /// containing category in registry order, which is the one the C3
    /// `categories-off` verdict blames and the one the Settings UI groups the
    /// server under, so all three name the same category for a multi-category
    /// server. `None` for an uncategorized server, for every non-MCP kind, and
    /// for every pre-V37 row.
    #[serde(default)]
    pub category: Option<String>,
}

impl ActivityEntry {
    /// The one way recorders build an entry: `kind` is the closed enum (no
    /// free-form strings at call sites) and `id` is always store-assigned.
    ///
    /// **`tab` is a required argument, not a defaulted one** (#51). The
    /// alternative — a defaulting constructor plus an opt-in `with_tab` — is
    /// the exact shape #47 removed from `record_flag`: a new call site would
    /// inherit "unattributed" by writing nothing, and the column whose entire
    /// purpose is telling you which tab did something would quietly stop
    /// answering as new recorders were added. Passing
    /// [`Attribution::Unattributed`] explicitly is fine; passing it by omission
    /// is not.
    ///
    /// **`server` and `category` are required for the same reason** (V37 C7).
    /// They are identity columns: a recorder that omits them produces a row that
    /// *looks* answered ("no server") when the truth is "nobody asked". Every
    /// call site that has no MCP server to name passes `None, None` explicitly,
    /// so adding a recorder is a decision about identity rather than a default
    /// inherited by silence. Old JSONL rows still parse — the `#[serde(default)]`
    /// on the fields is the READ path, which is a different question from what a
    /// writer may leave out.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: ActivityKind,
        ts_ms: u64,
        root: String,
        source: String,
        tool: String,
        target: String,
        chars: usize,
        ms: u64,
        ok: bool,
        tab: Attribution,
        session: Option<String>,
        server: Option<String>,
        category: Option<String>,
    ) -> Self {
        let mut entry = Self {
            id: 0,
            ts_ms,
            kind: kind.as_str().to_string(),
            root,
            source,
            tool,
            target,
            chars,
            ms,
            ok,
            // Classified two lines down, once the fields it reads are in place.
            status: RowStatus::unclassified(),
            tab,
            session,
            server,
            category,
        };
        entry.status = RowStatus::classify(&entry);
        entry
    }
}

/// Parse one stored JSONL line into a record, **re-deriving its status**.
///
/// The one function that turns a line into an [`ActivityRecord`], because the
/// status is derived rather than stored (see [`ActivityEntry::status`]): a row
/// written before the column existed has no word at all, and a row written by
/// an older build may carry one that build's classifier chose. Both re-derive
/// here, so what the feeds render is this build's reading of the row — which is
/// behaviour-preserving for every file already on disk, since the classifier
/// reads only columns those rows already carry.
fn parse_line(line: &str) -> serde_json::Result<ActivityRecord> {
    let mut rec: ActivityRecord = serde_json::from_str(line)?;
    rec.entry.status = RowStatus::classify(&rec.entry);
    Ok(rec)
}

fn default_kind() -> String {
    ActivityKind::Graph.as_str().to_string()
}

/// The full stored record: the light entry plus the captured (truncated)
/// request/response payloads. This is what's persisted per JSONL line and
/// what the on-demand `activity_detail` IPC returns.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ActivityRecord {
    #[serde(flatten)]
    pub entry: ActivityEntry,
    #[serde(default)]
    pub request: String,
    #[serde(default)]
    pub response: String,
}

struct StoreInner {
    loaded: bool,
    /// Kept sorted by `ts_ms` ascending (ties keep insertion order), so
    /// "oldest of a kind" is simply the front-most match.
    ring: VecDeque<ActivityRecord>,
    /// Per-process sequence feeding id generation. Seeded randomly so two
    /// processes appending to the same file (app + `--offload-mcp` child)
    /// don't both start their id residues at 0.
    seq: u64,
    /// Lines currently in the on-disk file (kept + appended); drives the
    /// periodic compaction.
    file_lines: usize,
}

/// The store: an in-memory ring mirrored to a JSONL file. Constructed with an
/// explicit path so tests get their own isolated file; production goes
/// through the [`store`] global.
pub struct ActivityStore {
    path: PathBuf,
    inner: Mutex<StoreInner>,
}

impl ActivityStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            inner: Mutex::new(StoreInner {
                loaded: false,
                ring: VecDeque::new(),
                seq: uuid::Uuid::new_v4().as_u128() as u64 % 1000,
                file_lines: 0,
            }),
        }
    }

    /// Append `rec` (payloads truncated, id assigned when 0) and mirror it to
    /// disk. The oldest entries OF ITS KIND are dropped past that kind's cap.
    ///
    /// Does synchronous file I/O — see the module's blocking note.
    pub fn record(&self, mut rec: ActivityRecord) {
        rec.request = truncate_chars(&rec.request, REQUEST_CAP_CHARS);
        rec.response = truncate_chars(&rec.response, RESPONSE_CAP_CHARS);
        // Re-classified at the funnel, not trusted from the caller: a recorder
        // that edits a field after `ActivityEntry::new` (the lane tests do)
        // would otherwise store a word derived from the row it used to be.
        rec.entry.status = RowStatus::classify(&rec.entry);
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        self.load_locked(&mut inner);
        if rec.entry.id == 0 {
            rec.entry.id = next_id(&mut inner);
        }
        // Serialize before the move into the ring — the append path then
        // needs no clone of the (payload-heavy) record.
        let line = match serde_json::to_string(&rec) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(error = %e, "activity: serialize record failed");
                None
            }
        };
        insert_sorted(&mut inner.ring, rec);
        enforce_lane_caps(&mut inner.ring);
        if inner.file_lines >= FILE_COMPACT_LINES {
            // Compaction rewrites the whole file; fold in any lines another
            // process appended first so the rewrite can't clobber them.
            self.merge_from_disk_locked(&mut inner);
            self.rewrite_locked(&mut inner);
        } else if let Some(line) = line {
            if self.append_line(&line) {
                inner.file_lines += 1;
            }
        }
    }

    /// A newest-first, payload-free snapshot of the entries with
    /// `ts_ms > since` (pass 0 for everything).
    ///
    /// Deliberately has no filtering counterpart — see `activity_list` in
    /// `ipc::commands` for why the Events tab narrows client-side instead.
    pub fn snapshot_since(&self, since: u64) -> Vec<ActivityEntry> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        self.load_locked(&mut inner);
        inner
            .ring
            .iter()
            .rev()
            .filter(|r| r.entry.ts_ms > since)
            .map(|r| r.entry.clone())
            .collect()
    }

    /// Full records (with payloads) whose `source` is one of `sources`,
    /// newest first.
    ///
    /// **Why this exists rather than `snapshot_since` + N × [`detail`]** (step
    /// 5). A row's `source` is on the light entry, but everything that makes a
    /// V32 flag row *joinable* — the `agent:tab` scope, the session, the host —
    /// lives only in its request payload. A consumer that needs those for a
    /// whole class of rows would otherwise take the full feed and then one
    /// locked `detail` call per row, which is O(rows) lock acquisitions and
    /// O(feed) serialization to answer a question about a handful of rows. The
    /// alternative — parsing the scope back out of the entry's `target`, which
    /// is a display string (`"{host} ({scope})"`) — makes a rendering decision
    /// load-bearing for a security surface.
    ///
    /// `sources` is small and compared linearly; every caller passes one or two.
    pub fn records_of_source(&self, sources: &[&str]) -> Vec<ActivityRecord> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        self.load_locked(&mut inner);
        inner
            .ring
            .iter()
            .rev()
            .filter(|r| sources.contains(&r.entry.source.as_str()))
            .cloned()
            .collect()
    }

    /// The full record (with payloads) for one entry, if it still exists.
    pub fn detail(&self, id: u64) -> Option<ActivityRecord> {
        let mut inner = self.inner.lock().ok()?;
        self.load_locked(&mut inner);
        inner.ring.iter().find(|r| r.entry.id == id).cloned()
    }

    /// Remove one entry by id (rewrites the file). Returns whether it existed.
    /// Re-merges foreign on-disk lines first so a concurrent child's appends
    /// survive the rewrite.
    pub fn delete(&self, id: u64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        self.load_locked(&mut inner);
        self.merge_from_disk_locked(&mut inner);
        let before = inner.ring.len();
        inner.ring.retain(|r| r.entry.id != id);
        let removed = inner.ring.len() != before;
        if removed {
            self.rewrite_locked(&mut inner);
        }
        removed
    }

    /// Drop every entry and truncate the file. Deliberately does NOT merge
    /// from disk first: the user asked for the history to be wiped, and that
    /// includes lines appended by another process.
    pub fn clear(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        self.load_locked(&mut inner);
        inner.ring.clear();
        self.rewrite_locked(&mut inner);
    }

    /// One-time lazy load of the on-disk mirror. Unparseable lines are
    /// skipped; pre-id (legacy in-memory era) lines get ids assigned;
    /// duplicate ids from a cross-process collision are repaired; an
    /// over-cap or repaired file is compacted/rewritten.
    fn load_locked(&self, inner: &mut StoreInner) {
        if inner.loaded {
            return;
        }
        inner.loaded = true;
        let Ok(text) = fs::read_to_string(&self.path) else {
            // Absent file: fresh install or first run after the upgrade.
            return;
        };
        let mut total_lines = 0usize;
        let mut repaired = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            total_lines += 1;
            match parse_line(line) {
                Ok(rec) => inner.ring.push_back(rec),
                Err(e) => {
                    repaired = true;
                    tracing::warn!(error = %e, "activity: skipping unparseable history line");
                }
            }
        }
        // File append order is usually time order, but two writers make no
        // guarantee — restore the sorted invariant explicitly.
        inner.ring.make_contiguous().sort_by_key(|r| r.entry.ts_ms);
        // Assign missing ids and repair duplicates (two processes can — very
        // rarely — mint the same id; `delete`'s retain would then drop both).
        let mut seen = HashSet::new();
        for i in 0..inner.ring.len() {
            let id = inner.ring[i].entry.id;
            if id == 0 || !seen.insert(id) {
                let fresh = next_id(inner);
                inner.ring[i].entry.id = fresh;
                seen.insert(fresh);
                repaired = true;
            }
        }
        if enforce_lane_caps(&mut inner.ring) {
            repaired = true;
        }
        inner.file_lines = total_lines;
        if repaired {
            self.rewrite_locked(inner);
        }
    }

    /// Fold on-disk lines this process has never seen (appended by an
    /// `--offload-mcp` child) into the ring, so an upcoming rewrite doesn't
    /// discard them. No-op when the file matches the ring.
    fn merge_from_disk_locked(&self, inner: &mut StoreInner) {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let known: HashSet<u64> = inner.ring.iter().map(|r| r.entry.id).collect();
        let mut merged = false;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(rec) = parse_line(line) else {
                continue;
            };
            // id 0 would collide as a set key; such lines only exist in
            // legacy files, which load_locked already repaired in the
            // process that owns them — skip rather than re-import.
            if rec.entry.id != 0 && !known.contains(&rec.entry.id) {
                insert_sorted(&mut inner.ring, rec);
                merged = true;
            }
        }
        if merged {
            enforce_lane_caps(&mut inner.ring);
        }
    }

    /// Best-effort single-line append of a pre-serialized record. Returns
    /// whether the write succeeded.
    fn append_line(&self, line: &str) -> bool {
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let res = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            tracing::warn!(error = %e, path = %self.path.display(), "activity: append failed");
            return false;
        }
        true
    }

    /// Rewrite the whole file from the ring (atomic replace).
    fn rewrite_locked(&self, inner: &mut StoreInner) {
        let mut out = String::new();
        for rec in inner.ring.iter() {
            match serde_json::to_string(rec) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(e) => tracing::warn!(error = %e, "activity: serialize record failed"),
            }
        }
        if let Err(e) = crate::settings::write_atomic(&self.path, out.as_bytes()) {
            tracing::warn!(error = %e, path = %self.path.display(), "activity: rewrite failed");
        }
        inner.file_lines = inner.ring.len();
    }
}

/// Insert keeping the ring sorted by `ts_ms` ascending (ties append after —
/// stable with respect to arrival order).
fn insert_sorted(ring: &mut VecDeque<ActivityRecord>, rec: ActivityRecord) {
    let mut idx = ring.len();
    while idx > 0 && ring[idx - 1].entry.ts_ms > rec.entry.ts_ms {
        idx -= 1;
    }
    ring.insert(idx, rec);
}

/// Drop the oldest entries of any [`Lane`] that exceeds its cap (the ring is
/// ts-sorted, so front-most = oldest). Returns whether anything was removed.
///
/// Every removal is charged to the lane it came from and to no other: the pass
/// counts per lane, then walks oldest-first dropping only rows whose *own* lane
/// is still over. So a source that floods pays for it out of its own window,
/// and the total is bounded by the sum of the lane caps ([`TOTAL_CAPACITY`])
/// without any counter having to track the total.
///
/// One pass, no sorting, no allocation beyond the count map — it runs under the
/// store lock on every `record`.
fn enforce_lane_caps(ring: &mut VecDeque<ActivityRecord>) -> bool {
    let mut counts: HashMap<Lane, usize> = HashMap::new();
    for r in ring.iter() {
        *counts.entry(Lane::of(&r.entry)).or_default() += 1;
    }
    let mut removed = false;
    let mut i = 0;
    while i < ring.len() {
        let lane = Lane::of(&ring[i].entry);
        let count = counts.get_mut(&lane).expect("counted above");
        if *count > lane.cap() {
            *count -= 1;
            ring.remove(i);
            removed = true;
        } else {
            i += 1;
        }
    }
    removed
}

/// Timestamp-based id: unique within the process via `seq` (randomly seeded
/// per process, so two writers to the same file don't share residues), and
/// double-checked against the local ring. A cross-process same-millisecond
/// collision is still theoretically possible; the load path repairs any that
/// make it into one file. Stays below 2^53 so it round-trips through the JS
/// frontend as a number.
fn next_id(inner: &mut StoreInner) -> u64 {
    loop {
        let id = now_ms() * 1000 + inner.seq % 1000;
        inner.seq += 1;
        if id != 0 && !inner.ring.iter().any(|r| r.entry.id == id) {
            return id;
        }
    }
}

/// Cut `s` to at most `cap` chars, with an explicit truncation marker.
fn truncate_chars(s: &str, cap: usize) -> String {
    // Fast path: byte length bounds char count from above, so a short-by-bytes
    // string can never exceed the char cap — skip the full decode.
    if s.len() <= cap {
        return s.to_string();
    }
    let total = s.chars().count();
    if total <= cap {
        return s.to_string();
    }
    let kept: String = s.chars().take(cap).collect();
    format!("{kept}\n… [truncated {} chars]", total - cap)
}

/// `<exe-dir>/tool-activity.jsonl` — the portable location, same rationale as
/// `settings::global_path`. Falls back to the working directory when
/// `current_exe` is unavailable.
fn default_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(FILE_NAME)
}

static STORE: LazyLock<ActivityStore> = LazyLock::new(|| {
    // Pin the process-start timestamp no later than the first store access:
    // "since this run" consumers (effectiveness_totals) compare entry
    // timestamps against it to exclude restored pre-restart entries.
    let _ = process_start_ms();
    ActivityStore::new(default_path())
});

fn store() -> &'static ActivityStore {
    &STORE
}

/// Append an activity record to the process-wide store (synchronous file
/// I/O — do not call directly from async contexts; see [`record_bg`]).
pub fn record(rec: ActivityRecord) {
    store().record(rec);
}

/// [`record`] moved off the async runtime: on a tokio thread the write runs
/// via `spawn_blocking` (fire-and-forget — an entry lost to a process exit
/// mid-flight is acceptable for an activity log); outside a runtime it
/// records inline.
pub fn record_bg(rec: ActivityRecord) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(move || record(rec));
        }
        Err(_) => record(rec),
    }
}

/// A newest-first, payload-free snapshot (all kinds).
pub fn snapshot() -> Vec<ActivityEntry> {
    store().snapshot_since(0)
}

/// A newest-first, payload-free snapshot of entries with `ts_ms > since` —
/// lets the 1.5–2s pollers skip re-serializing hundreds of rows they already
/// have.
pub fn snapshot_since(since: u64) -> Vec<ActivityEntry> {
    store().snapshot_since(since)
}

/// The full record (with request/response payloads) for one entry.
pub fn detail(id: u64) -> Option<ActivityRecord> {
    store().detail(id)
}

/// Full records (with payloads) for every row written by one of `sources`,
/// newest first — see [`ActivityStore::records_of_source`].
pub fn records_of_source(sources: &[&str]) -> Vec<ActivityRecord> {
    store().records_of_source(sources)
}

/// Remove one entry by id.
pub fn delete(id: u64) -> bool {
    store().delete(id)
}

/// Drop the whole history.
pub fn clear() {
    store().clear()
}

static PROCESS_START_MS: LazyLock<u64> = LazyLock::new(now_ms);

/// Epoch millis captured at (or before) the first store access this process —
/// the "since restart" cutoff for consumers that must not count entries
/// restored from disk.
pub fn process_start_ms() -> u64 {
    *PROCESS_START_MS
}

/// Memo for [`root_key`]: `canonicalize` is a real filesystem syscall and the
/// key is requested on every recorded tool call and advisor/auto-check event,
/// always for the same small set of roots (plus a few per-tab cwds) — cache
/// the successful answers. Failures (e.g. a vanished directory) are NOT
/// cached, so a transient error can't stick.
static ROOT_KEYS: LazyLock<Mutex<HashMap<PathBuf, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The canonical string form a root is recorded (and filtered) under.
/// Recorders and the scoped `graph_history` filter must both go through this:
/// the same directory reaches them in different spellings (the launch cwd vs.
/// a `find_graph_root` ancestor walk, drive-letter vs. verbatim `\\?\` form on
/// Windows), and canonicalizing both sides makes those compare equal. Falls
/// back to the path as given (e.g. the directory vanished mid-call).
///
/// **#104 item 5: the verbatim prefix is stripped.** `canonicalize` yields the
/// extended-length form on Windows and the fallback arm yields whatever the
/// caller spelled, so ONE project reached the store under two keys and a scoped
/// reader filtering on either spelling silently dropped the other's rows. Both
/// arms now go through [`crate::fsutil::plain_path`], so the recorded key is the
/// plain drive-letter form — the spelling the user sees and the one most
/// existing rows already carry. Rows written under the old verbatim spelling
/// stay readable because every comparison goes through [`root_key_eq`], which
/// normalizes the STORED side too.
pub fn root_key(root: &Path) -> String {
    if let Ok(cache) = ROOT_KEYS.lock() {
        if let Some(key) = cache.get(root) {
            return key.clone();
        }
    }
    match std::fs::canonicalize(root) {
        Ok(canon) => {
            let key = crate::fsutil::plain_path(&canon.to_string_lossy());
            if let Ok(mut cache) = ROOT_KEYS.lock() {
                // Bounded memo: the key set is tiny in practice; a wholesale
                // clear on the (unexpected) way past the cap keeps it O(1)
                // without needing an eviction policy.
                if cache.len() >= 256 {
                    cache.clear();
                }
                cache.insert(root.to_path_buf(), key.clone());
            }
            key
        }
        Err(_) => crate::fsutil::plain_path(&root.to_string_lossy()),
    }
}

/// Whether two recorded root keys name the SAME project.
///
/// #104 item 5. Never compare `entry.root` with `==`: the store outlives the
/// build that wrote it, so it holds rows in the pre-fix extended-length
/// (verbatim) spelling alongside rows in the plain one, and a raw string
/// compare splits one project into two lanes — the scoped `graph_history`
/// filter, the advisor's per-root retain, and the H1 ambiguity predicate all
/// read one lane and miss the other. Normalizes both sides (verbatim prefix
/// stripped by [`crate::fsutil::plain_path`], then
/// [`crate::fsutil::norm_dir_key`]'s separator/trailing-separator/case
/// folding), so an old row and a new one for one directory compare equal.
///
/// An empty key equals only an empty key: the empty string is the honest
/// "attributed to no project" value (an agent cwd that resolved to no root —
/// #104 item 2), not a wildcard.
pub fn root_key_eq(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return a == b;
    }
    crate::fsutil::norm_dir_key(&crate::fsutil::plain_path(a))
        == crate::fsutil::norm_dir_key(&crate::fsutil::plain_path(b))
}

/// Current Unix epoch in milliseconds (0 if the clock is before the epoch).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(target: &str) -> ActivityRecord {
        rec_kind(ActivityKind::Graph, target)
    }

    /// An `injection_flag` row as the real writers build one: the `source`
    /// column carries the SCREEN (`outbound::record_flag` stamps
    /// `Screen::as_str()`), which is what puts the row in its retention lane.
    /// `rec_kind` alone would produce a row sourced `"offload"` — a real kind
    /// with no real screen, i.e. the one shape no writer emits.
    fn rec_flag(screen: Screen, target: &str) -> ActivityRecord {
        let mut r = rec_kind(ActivityKind::InjectionFlag, target);
        r.entry.source = screen.as_str().to_string();
        r
    }

    /// How many rows one screen currently holds.
    fn screen_rows(snap: &[ActivityEntry], screen: Screen) -> usize {
        snap.iter()
            .filter(|e| {
                e.kind == ActivityKind::InjectionFlag.as_str() && e.source == screen.as_str()
            })
            .count()
    }

    /// Whether a row with this exact `target` survived. The forensic assertions
    /// are by CONTENT, never by count: a count is satisfied by any 200 rows,
    /// including the 200 that replaced the evidence.
    fn kept(snap: &[ActivityEntry], target: &str) -> bool {
        snap.iter().any(|e| e.target == target)
    }

    fn rec_kind(kind: ActivityKind, target: &str) -> ActivityRecord {
        ActivityRecord {
            entry: ActivityEntry::new(
                kind,
                now_ms(),
                root_key(Path::new(".")),
                "offload".into(),
                "graph_outline".into(),
                target.into(),
                0,
                0,
                true,
                Attribution::Unattributed,
                None,
                None,
                None,
            ),
            request: format!("{{\"file\": \"{target}\"}}"),
            response: format!("outline of {target}"),
        }
    }

    // ── #51: row attribution ─────────────────────────────────────────────

    /// **The backward-compatibility guarantee.** Every row in an existing
    /// `tool-activity.jsonl` was written without these fields, and must come
    /// back as "we weren't recording it yet" — never as `Headless`, which is a
    /// positive claim that nobody was on a tab.
    ///
    /// Both facts are load-bearing and only one of them is evidence: a
    /// containment row that reads `Headless` says the caller had no tab, which
    /// a reviewer would take as a finding. An old row must not be able to
    /// manufacture that.
    #[test]
    fn a_row_written_before_the_columns_existed_is_unattributed_not_headless() {
        let legacy = r#"{"id":7,"ts_ms":1,"kind":"graph","root":"r","source":"claude",
            "tool":"graph_outline","target":"x","chars":3,"ms":4,"ok":true}"#;
        let e: ActivityEntry = serde_json::from_str(legacy).expect("legacy row parses");

        assert_eq!(e.tab, Attribution::Unattributed);
        assert_ne!(
            e.tab,
            Attribution::Headless,
            "an unrecorded tab must never read as a positive `no tab` claim"
        );
        assert_eq!(e.session, None);
        // …and the rest of the row is untouched.
        assert_eq!(e.tool, "graph_outline");
        assert!(e.ok);
    }

    /// `Unrecognized` is an id the caller quoted, not a tab that exists —
    /// `loopback::tab_identity`'s `Unknown` "creates no row and gates nothing".
    /// A filter-by-tab that matched it would attribute activity to a tab that
    /// does not exist, inside the view whose whole job is attribution.
    #[test]
    fn only_a_configured_tab_counts_as_a_tab() {
        assert!(Attribution::Tab("claude".into()).is_tab());
        assert!(!Attribution::Unrecognized("claude".into()).is_tab());
        assert!(!Attribution::Headless.is_tab());
        assert!(!Attribution::Unattributed.is_tab());

        // Both id-carrying states still surface the id — the UI has to be able
        // to say *which* id was unrecognized.
        assert_eq!(Attribution::Tab("a".into()).id(), Some("a"));
        assert_eq!(Attribution::Unrecognized("b".into()).id(), Some("b"));
        assert_eq!(Attribution::Headless.id(), None);
        assert_eq!(Attribution::Unattributed.id(), None);
    }

    /// An argv tab is trusted; its ABSENCE is a fact, not a gap.
    ///
    /// The `None` case is the one worth pinning: it must be `Headless`, because
    /// a child with no `--tab` was not spawned by cImp at all. Returning
    /// `Unattributed` there would say "we don't know" about something we do
    /// know, and would make a headless `claude -p` call indistinguishable from
    /// a row written before the column existed.
    ///
    /// #48 F-20 narrows WHERE this constructor may be called: only at entry points
    /// that actually hold argv (`graph::mcp::handle_call`). App-side recorders now
    /// receive an already-classified `Attribution` — the loopback route's comes
    /// from `loopback::LatchScoping::attribution`, which can answer
    /// `Unrecognized`, a state this constructor cannot produce and must not.
    #[test]
    fn an_argv_tab_is_a_real_tab_and_its_absence_is_headless() {
        assert_eq!(
            Attribution::from_child_argv(Some("claude")),
            Attribution::Tab("claude".into())
        );
        assert_eq!(Attribution::from_child_argv(None), Attribution::Headless);
        // Whitespace-only is absence, matching `loopback::tab_identity`.
        assert_eq!(Attribution::from_child_argv(Some("   ")), Attribution::Headless);
        assert_eq!(
            Attribution::from_child_argv(Some(" claude ")),
            Attribution::Tab("claude".into())
        );
    }

    #[test]
    fn attribution_round_trips_through_the_wire() {
        for a in [
            Attribution::Unattributed,
            Attribution::Headless,
            Attribution::Tab("claude".into()),
            Attribution::Unrecognized("nope".into()),
        ] {
            let j = serde_json::to_string(&a).expect("serialize");
            let back: Attribution = serde_json::from_str(&j).expect("deserialize");
            assert_eq!(a, back, "round trip via {j}");
        }
    }

    fn temp_store(name: &str) -> ActivityStore {
        let path = std::env::temp_dir()
            .join("cimp-activity-tests")
            .join(format!("{name}-{}.jsonl", uuid::Uuid::new_v4()));
        ActivityStore::new(path)
    }

    #[test]
    fn root_key_is_stable_across_spellings() {
        // The same directory reached via different relative spellings must
        // produce the same key — that equality is what the scoped
        // `graph_history` filter relies on.
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(root_key(&cwd), root_key(Path::new(".")));
        // A path that doesn't exist falls back to its literal form.
        assert_eq!(
            root_key(Path::new("definitely/not/a/real/dir")),
            Path::new("definitely/not/a/real/dir").to_string_lossy()
        );
    }

    /// #104 item 5: one project, ONE lane. `canonicalize` returns the
    /// extended-length (verbatim) form on Windows while every configured path,
    /// every fallback arm and every pre-fix row carries the plain one, so the
    /// store held both spellings for the same directory and a scoped reader
    /// filtering on either dropped the other's rows.
    #[test]
    fn both_spellings_of_one_root_map_to_one_key() {
        let cwd = std::env::current_dir().expect("cwd");
        let key = root_key(&cwd);
        // The key itself is the plain spelling — the one the user sees.
        assert!(
            !key.starts_with(VERBATIM),
            "the recorded key must not carry the verbatim prefix: {key}"
        );
        // And a row written by a pre-fix build still resolves to this project.
        let verbatim = format!("{VERBATIM}{}", key.trim_start_matches(VERBATIM));
        assert!(
            root_key_eq(&verbatim, &key),
            "an old verbatim row must still match {key}"
        );
        // Separator and case differences fold too (Windows paths are
        // case-insensitive), and a trailing separator is not a second project.
        assert!(root_key_eq(&key, &format!("{key}/")));
        if cfg!(windows) {
            assert!(root_key_eq(&key.to_uppercase(), &key));
        }
        // An empty key is "attributed to no project", never a wildcard.
        assert!(root_key_eq("", ""));
        assert!(!root_key_eq("", &key));
        assert!(!root_key_eq(&key, ""));
        // Two genuinely different projects stay different.
        assert!(!root_key_eq("P:/proj/a", "P:/proj/b"));
    }

    /// Windows' extended-length prefix (`\\?\`), spelled once here.
    const VERBATIM: &str = "\\\\?\\";

    #[test]
    fn records_newest_first_within_cap() {
        let store = temp_store("order");
        for t in ["a0", "a1", "a2"] {
            store.record(rec(t));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].target, "a2");
        assert_eq!(snap[1].target, "a1");
        assert_eq!(snap[2].target, "a0");
        // Every entry got a distinct non-zero id.
        assert!(snap.iter().all(|e| e.id != 0));
        assert_ne!(snap[0].id, snap[1].id);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn persists_across_reload() {
        let store = temp_store("persist");
        store.record(rec("kept"));
        let id = store.snapshot_since(0)[0].id;

        // A fresh store on the same path (a "restart") sees the entry, with
        // payloads intact.
        let reloaded = ActivityStore::new(store.path.clone());
        let snap = reloaded.snapshot_since(0);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].target, "kept");
        assert_eq!(snap[0].id, id);
        let full = reloaded.detail(id).expect("detail");
        assert!(full.request.contains("kept"));
        assert!(full.response.contains("outline of kept"));
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn delete_and_clear_persist() {
        let store = temp_store("delete");
        store.record(rec("a"));
        store.record(rec("b"));
        let snap = store.snapshot_since(0);
        let (b_id, a_id) = (snap[0].id, snap[1].id);

        assert!(store.delete(a_id));
        assert!(!store.delete(a_id), "second delete is a no-op");
        let after = ActivityStore::new(store.path.clone()).snapshot_since(0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, b_id);

        store.clear();
        assert!(store.snapshot_since(0).is_empty());
        assert!(ActivityStore::new(store.path.clone())
            .snapshot_since(0)
            .is_empty());
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn load_compacts_past_kind_cap() {
        let store = temp_store("cap");
        for i in 0..(GRAPH_CAP + 25) {
            store.record(rec(&format!("t{i}")));
        }
        assert_eq!(store.snapshot_since(0).len(), GRAPH_CAP);
        // Reload keeps only the newest GRAPH_CAP, newest first.
        let reloaded = ActivityStore::new(store.path.clone());
        let snap = reloaded.snapshot_since(0);
        assert_eq!(snap.len(), GRAPH_CAP);
        assert_eq!(snap[0].target, format!("t{}", GRAPH_CAP + 24));
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn offload_entries_survive_a_graph_flood() {
        // The caps are per kind: a graph-heavy session must never evict the
        // (rare, valuable) offload rows — the crowd-out the old split rings
        // existed to prevent.
        let store = temp_store("flood");
        store.record(rec_kind(ActivityKind::Offload, "the offload run"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "offload").count(),
            1,
            "offload entry was evicted by graph traffic"
        );
        assert_eq!(snap.iter().filter(|e| e.kind == "graph").count(), GRAPH_CAP);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn a_server_lifecycle_row_survives_both_floods() {
        // The reason `offload_server` is its own lane: the rows that explain a
        // bad session (a backend failing, dying, being restarted) are written
        // by a backend that is BY DEFINITION not producing task rows, so both
        // the chatty graph feed and the offload task feed are the traffic that
        // would bury them.
        let store = temp_store("server-lane");
        store.record(rec_kind(ActivityKind::OffloadServer, "exited unexpectedly"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        for i in 0..(OFFLOAD_CAP + 50) {
            store.record(rec_kind(ActivityKind::Offload, &format!("run {i}")));
        }
        let snap = store.snapshot_since(0);
        let rows: Vec<_> = snap
            .iter()
            .filter(|e| e.kind == "offload_server")
            .collect();
        assert_eq!(rows.len(), 1, "lifecycle row was evicted by other feeds");
        assert_eq!(rows[0].target, "exited unexpectedly");
        assert_eq!(snap.iter().filter(|e| e.kind == "offload").count(), OFFLOAD_CAP);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn server_lifecycle_rows_are_capped_at_their_own_window() {
        // A crash-restart loop is the flood this lane must bound: it writes
        // lifecycle rows as fast as the process can die, and must still not be
        // able to grow the store without limit.
        let store = temp_store("server-cap");
        for i in 0..(OFFLOAD_SERVER_CAP + 40) {
            store.record(rec_kind(ActivityKind::OffloadServer, &format!("boot {i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "offload_server").count(),
            OFFLOAD_SERVER_CAP
        );
        // Newest kept, oldest evicted.
        assert_eq!(
            snap.iter()
                .find(|e| e.kind == "offload_server")
                .map(|e| e.target.as_str()),
            Some(format!("boot {}", OFFLOAD_SERVER_CAP + 39).as_str())
        );
        let _ = fs::remove_file(&store.path);
    }

    /// V38 Phase A: the `plugin` lane, both directions.
    ///
    /// The row that matters here is the one saying a manifest was REJECTED —
    /// it is minted once per scan and then never again until the user rescans,
    /// while the graph and offload feeds around it write continuously. That is
    /// the exact crowd-out shape #51 says to pick a lane for, so the assertion
    /// is by CONTENT (did the rejection survive?), not by count.
    #[test]
    fn plugin_rows_keep_their_own_window() {
        let store = temp_store("plugin-lane");
        store.record(rec_kind(ActivityKind::Plugin, "rejected: acme@1.0.0"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        for i in 0..(OFFLOAD_CAP + 50) {
            store.record(rec_kind(ActivityKind::Offload, &format!("run {i}")));
        }
        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "rejected: acme@1.0.0"),
            "a plugin rejection was evicted by the chatty feeds it shares no lane with"
        );

        // …and the lane is bounded: a user re-scanning a broken folder in a
        // loop must not be able to grow the store without limit.
        for i in 0..(PLUGIN_CAP + 40) {
            store.record(rec_kind(ActivityKind::Plugin, &format!("scan {i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "plugin").count(),
            PLUGIN_CAP
        );
        // Newest kept, oldest evicted.
        assert_eq!(
            snap.iter()
                .find(|e| e.kind == "plugin")
                .map(|e| e.target.as_str()),
            Some(format!("scan {}", PLUGIN_CAP + 39).as_str())
        );
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn mcp_entries_keep_their_own_window() {
        // MCP rows get their own retention window (MCP_CAP), independent of
        // graph traffic in both directions.
        let store = temp_store("mcp-cap");
        store.record(rec_kind(ActivityKind::Mcp, "an mcp call"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        for i in 0..(MCP_CAP + 10) {
            store.record(rec_kind(ActivityKind::Mcp, &format!("m{i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(snap.iter().filter(|e| e.kind == "mcp").count(), MCP_CAP);
        assert_eq!(snap.iter().filter(|e| e.kind == "graph").count(), GRAPH_CAP);
        let _ = fs::remove_file(&store.path);
    }

    /// V32: security denials are the only consumer of every containment
    /// refusal, so a chatty session must never evict them.
    ///
    /// # This test used to assert the breach (#48, finding H-9)
    ///
    /// It planted a row targeted `"a denial"`, flooded past the cap, and then
    /// checked only that the count equalled the cap — which is true *precisely
    /// because* the planted row had been evicted, and it never looked. The
    /// assertions it was missing are the three below: the planted row survives
    /// a flood of another KIND, it survives a flood of another SCREEN, and it
    /// is its own screen's newer rows — nobody else's — that finally retire it.
    #[test]
    fn injection_flag_entries_keep_their_own_window() {
        let store = temp_store("flag-cap");
        store.record(rec_flag(Screen::Canary, "a denial"));
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "injection_flag").count(),
            1,
            "a graph flood evicted the security denials"
        );
        assert!(
            kept(&snap, "a denial"),
            "the planted denial row was evicted by graph traffic — the count above cannot see that"
        );

        // A flood of a DIFFERENT screen must not touch it either: same kind,
        // different lane.
        for i in 0..(INJECTION_FLAG_SCREEN_CAP + 10) {
            store.record(rec_flag(Screen::MemoryQuarantine, &format!("q{i}")));
        }
        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "a denial"),
            "a memory-quarantine flood evicted the canary row"
        );
        assert_eq!(
            screen_rows(&snap, Screen::MemoryQuarantine),
            INJECTION_FLAG_SCREEN_CAP,
            "the flooding screen must be bounded by its own lane cap"
        );

        // And its own screen's newer rows are what finally retire it — the one
        // eviction the design allows.
        for i in 0..(INJECTION_FLAG_SCREEN_CAP + 10) {
            store.record(rec_flag(Screen::Canary, &format!("c{i}")));
        }
        let snap = store.snapshot_since(0);
        assert!(
            !kept(&snap, "a denial"),
            "a lane must still evict its own oldest — otherwise it is unbounded"
        );
        assert_eq!(
            screen_rows(&snap, Screen::Canary),
            INJECTION_FLAG_SCREEN_CAP
        );
        assert_eq!(snap.iter().filter(|e| e.kind == "graph").count(), GRAPH_CAP);
        let _ = fs::remove_file(&store.path);
    }

    /// **The H-9 exploit, literally.** A canary exfiltration is caught (a
    /// `Canary` row and the `LatchBeacon` row naming what engaged containment),
    /// and the model then issues `context_note` after `context_note` carrying an
    /// `AKIA…`-shaped literal. `context_note` has no fetch budget, no SSRF
    /// screen and no latch refusal to stop it, and the secret screen writes one
    /// `MemoryQuarantine` row per note.
    ///
    /// The flood is sized past the WHOLE feed's aggregate ceiling, so it is not
    /// a test of one particular shared number: any design that retains
    /// `injection_flag` rows in a single shared window loses the two planted
    /// rows here, whatever that window's size.
    ///
    /// Asserted by CONTENT. The pre-fix version of this store passes every
    /// count-shaped assertion in this file while holding nothing but quarantine
    /// rows.
    #[test]
    fn a_quarantine_flood_cannot_evict_the_forensic_trail() {
        let store = temp_store("h9-exploit");
        store.record(rec_flag(Screen::Canary, "canary in a fetched page"));
        store.record(rec_flag(Screen::LatchBeacon, "webfetch engaged the latch"));

        for i in 0..(INJECTION_FLAG_TOTAL_CAP + 8) {
            store.record(rec_flag(Screen::MemoryQuarantine, &format!("note {i}")));
        }

        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "canary in a fetched page"),
            "the note flood evicted the canary row — the only record of the exfiltration"
        );
        assert!(
            kept(&snap, "webfetch engaged the latch"),
            "the note flood evicted the beacon row — the only record of what engaged containment"
        );
        assert_eq!(
            screen_rows(&snap, Screen::MemoryQuarantine),
            INJECTION_FLAG_SCREEN_CAP,
            "the flood must be bounded by its own lane"
        );
        // Newest-first ordering is untouched by lane eviction — the UI reads
        // this snapshot in order.
        assert!(snap.windows(2).all(|w| w[0].ts_ms >= w[1].ts_ms));
        let _ = fs::remove_file(&store.path);
    }

    /// #48 (F-3): the same exploit aimed at the row that anchors everything
    /// else — the moment a tab became contaminated.
    ///
    /// It is one row per tab-session and the rarest thing in the feed, and every
    /// other containment event in that tab is only legible relative to it, so
    /// "the quarantine flood cannot reach it" is the property worth stating
    /// separately from the generated matrix below. The flood here is a
    /// `MemoryQuarantine` one *because* that is what a contaminated tab
    /// produces: the notes it writes after the transition are exactly the rows
    /// that would have evicted the record of the transition.
    #[test]
    fn a_quarantine_flood_cannot_evict_the_contamination_row() {
        let store = temp_store("f3-contamination");
        store.record(rec_flag(
            Screen::Contamination,
            "claude:claude-1 read a page",
        ));
        for i in 0..(INJECTION_FLAG_TOTAL_CAP + 8) {
            store.record(rec_flag(Screen::MemoryQuarantine, &format!("note {i}")));
        }
        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "claude:claude-1 read a page"),
            "the notes a contaminated tab writes evicted the record of its contamination"
        );
        assert_eq!(
            screen_rows(&snap, Screen::Contamination),
            1,
            "the transition row is one per tab-session and must not be duplicated by retention"
        );
        let _ = fs::remove_file(&store.path);
    }

    /// The flood in the other direction: the chatty screen is a FORENSIC one.
    ///
    /// A pinned/unpinned split would pass the test above and fail here — the
    /// quarantine is in the forensic set, so pinning it puts the flood inside
    /// the protected lane and the canary rows are evicted exactly as before.
    /// The property is per-screen, not per-privilege.
    #[test]
    fn a_chatty_forensic_screen_cannot_evict_a_rare_one() {
        let store = temp_store("h9-reverse");
        store.record(rec_flag(Screen::LatchOverride, "the user restored access"));
        for i in 0..(INJECTION_FLAG_TOTAL_CAP + 8) {
            store.record(rec_flag(Screen::Canary, &format!("canary hit {i}")));
        }
        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "the user restored access"),
            "a canary flood evicted the record of the user granting capability back"
        );
        assert_eq!(
            screen_rows(&snap, Screen::Canary),
            INJECTION_FLAG_SCREEN_CAP
        );
        let _ = fs::remove_file(&store.path);
    }

    /// The property itself, over the ENUM: **no screen can evict another
    /// screen's rows** — for every screen as the flooder, and every screen as
    /// the victim.
    ///
    /// Generated from `Screen::ALL`, which `declare_screens!` emits from the
    /// variant list, so a screen added tomorrow is covered as both roles
    /// without anyone extending this test. That is the same reason the lane set
    /// is derived from `ALL` rather than listed: F-3's contamination-event row
    /// had to inherit the guarantee by construction, not by being remembered —
    /// and it did, `Screen::Contamination` appearing in both roles here with no
    /// edit to this test.
    #[test]
    fn no_screen_can_evict_another_screens_rows() {
        for flooder in Screen::ALL.iter().copied() {
            let store = temp_store(&format!("h9-matrix-{}", flooder.as_str()));
            for victim in Screen::ALL.iter().copied() {
                store.record(rec_flag(victim, &format!("keep-{}", victim.as_str())));
            }
            for i in 0..(INJECTION_FLAG_SCREEN_CAP + 5) {
                store.record(rec_flag(flooder, &format!("flood-{i}")));
            }
            let snap = store.snapshot_since(0);
            for victim in Screen::ALL.iter().copied() {
                if victim == flooder {
                    continue;
                }
                assert!(
                    kept(&snap, &format!("keep-{}", victim.as_str())),
                    "a {} flood evicted the {} row",
                    flooder.as_str(),
                    victim.as_str()
                );
            }
            assert_eq!(
                screen_rows(&snap, flooder),
                INJECTION_FLAG_SCREEN_CAP,
                "{} exceeded its own lane cap",
                flooder.as_str()
            );
            let _ = fs::remove_file(&store.path);
        }
    }

    /// Bounded: every screen flooding at once still totals the sum of the lane
    /// caps and no more. Per-screen retention would be worthless if "its own
    /// lane" meant an unbounded one — the store is written to disk.
    #[test]
    fn the_injection_feed_is_bounded_under_a_flood_of_every_screen() {
        let store = temp_store("h9-bounded");
        for screen in Screen::ALL.iter().copied() {
            for i in 0..(2 * INJECTION_FLAG_SCREEN_CAP) {
                store.record(rec_flag(screen, &format!("{}-{i}", screen.as_str())));
            }
        }
        let snap = store.snapshot_since(0);
        for screen in Screen::ALL.iter().copied() {
            assert_eq!(
                screen_rows(&snap, screen),
                INJECTION_FLAG_SCREEN_CAP,
                "{} is not bounded by its lane cap",
                screen.as_str()
            );
        }
        let flags = snap.iter().filter(|e| e.kind == "injection_flag").count();
        assert_eq!(flags, Screen::ALL.len() * INJECTION_FLAG_SCREEN_CAP);
        assert!(flags <= INJECTION_FLAG_TOTAL_CAP);
        assert!(snap.len() <= TOTAL_CAPACITY);
        let _ = fs::remove_file(&store.path);
    }

    /// Non-starvation: a screen that legitimately produces a lot keeps its
    /// whole share, and keeps the NEWEST rows of it, while every other lane is
    /// also busy.
    #[test]
    fn a_screen_using_its_whole_share_keeps_its_newest_rows() {
        let store = temp_store("h9-starve");
        for i in 0..INJECTION_FLAG_SCREEN_CAP {
            store.record(rec_flag(Screen::LatchRefusal, &format!("refusal {i}")));
        }
        // Every other lane goes to work around it.
        for screen in Screen::ALL
            .iter()
            .copied()
            .filter(|s| *s != Screen::LatchRefusal)
        {
            for i in 0..(INJECTION_FLAG_SCREEN_CAP + 20) {
                store.record(rec_flag(screen, &format!("{}-{i}", screen.as_str())));
            }
        }
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            screen_rows(&snap, Screen::LatchRefusal),
            INJECTION_FLAG_SCREEN_CAP,
            "a screen inside its own share lost rows to other lanes"
        );
        for i in 0..INJECTION_FLAG_SCREEN_CAP {
            assert!(kept(&snap, &format!("refusal {i}")), "lost refusal {i}");
        }
        let _ = fs::remove_file(&store.path);
    }

    /// The guarantee has to survive the JSONL round-trip, because that is where
    /// the forensic rows actually live: the flood below is long enough to force
    /// at least one compaction (whole-file rewrite from the ring), and the
    /// assertions then run against a FRESH store loaded from the file.
    #[test]
    fn the_forensic_trail_survives_compaction_and_reload() {
        let store = temp_store("h9-compact");
        store.record(rec_flag(Screen::Canary, "canary in a fetched page"));
        store.record(rec_flag(Screen::LatchBeacon, "webfetch engaged the latch"));
        for i in 0..(FILE_COMPACT_LINES + 20) {
            store.record(rec_flag(Screen::MemoryQuarantine, &format!("note {i}")));
        }
        // The compaction did happen, and left room to append again rather than
        // rewriting the file on every subsequent record.
        {
            let inner = store.inner.lock().expect("lock");
            assert!(inner.file_lines <= FILE_COMPACT_LINES);
            assert!(inner.ring.len() <= TOTAL_CAPACITY);
        }

        let reloaded = ActivityStore::new(store.path.clone());
        let snap = reloaded.snapshot_since(0);
        assert!(
            kept(&snap, "canary in a fetched page"),
            "the canary row did not survive compaction + reload"
        );
        assert!(
            kept(&snap, "webfetch engaged the latch"),
            "the beacon row did not survive compaction + reload"
        );
        assert_eq!(
            screen_rows(&snap, Screen::MemoryQuarantine),
            INJECTION_FLAG_SCREEN_CAP
        );
        let _ = fs::remove_file(&store.path);
    }

    /// Rows whose `source` this build does not know — written by a newer
    /// version, or under a retired wire value — share ONE bounded lane. They
    /// stay bounded, and they cannot evict a screen this build does know.
    #[test]
    fn an_unrecognized_source_shares_one_bounded_lane() {
        let store = temp_store("h9-unknown");
        store.record(rec_flag(Screen::Canary, "canary in a fetched page"));
        for i in 0..(INJECTION_FLAG_SCREEN_CAP + 20) {
            let mut r = rec_kind(ActivityKind::InjectionFlag, &format!("future {i}"));
            r.entry.source = format!("a_screen_from_v33_{}", i % 3);
            store.record(r);
        }
        let snap = store.snapshot_since(0);
        assert!(
            kept(&snap, "canary in a fetched page"),
            "unrecognized-source rows evicted a known screen's row"
        );
        let unknown = snap
            .iter()
            .filter(|e| {
                e.kind == ActivityKind::InjectionFlag.as_str()
                    && Screen::from_wire(&e.source).is_none()
            })
            .count();
        assert_eq!(
            unknown, INJECTION_FLAG_SCREEN_CAP,
            "the catch-all lane must be bounded like any other"
        );
        let _ = fs::remove_file(&store.path);
    }

    /// The sentinel lane key can never be mistaken for a screen.
    #[test]
    fn the_unknown_lane_is_not_a_screen_name() {
        assert!(Screen::from_wire(UNKNOWN_SCREEN_LANE).is_none());
        assert!(Screen::ALL
            .iter()
            .all(|s| s.as_str() != UNKNOWN_SCREEN_LANE));
    }

    /// **R23's pin.** The kind table is the only enumerator of kinds, so it has
    /// to cover the enum exactly and answer for it consistently.
    ///
    /// * every ROW names a real variant — carried by the type (`kind:
    ///   ActivityKind`), and checked to be the variant whose slot it occupies;
    /// * every VARIANT has exactly one row — `declare_activity_kinds!` emits the
    ///   enum and the table from one list, so a variant without a row is a macro
    ///   parse error and `KINDS`' const assertion refuses to build on a
    ///   mis-ordered one. This is those guarantees restated where a reader looks
    ///   for them, plus the two they cannot make: that the wire keys are unique,
    ///   and that `as_str` really answers from the row.
    #[test]
    fn every_kind_has_exactly_one_row() {
        let mut seen: Vec<&str> = Vec::new();
        for (i, row) in KINDS.iter().enumerate() {
            assert!(!seen.contains(&row.key), "duplicate kind key {}", row.key);
            seen.push(row.key);
            assert_eq!(row.kind as usize, i, "row {i} names another kind");
            assert_eq!(row.kind.as_str(), row.key, "as_str disagrees with row {i}");
        }
    }

    /// **The capacity pin.** [`TOTAL_CAPACITY`] is folded out of [`KINDS`] now;
    /// this is the literal it used to be hand-summed to, asserted once so a cap
    /// edit is LOUD rather than a quietly larger ring.
    ///
    /// Only the ten per-kind windows are a literal. The `injection_flag` term
    /// stays symbolic on purpose: it is per-SOURCE-lane, and its lane count is
    /// `Screen::ALL.len() + 1` precisely so that a new screen gets a guaranteed
    /// window by existing (#48, H-9) — pinning a number here would turn that
    /// guarantee into a test failure.
    #[test]
    fn total_capacity_is_the_sum_the_table_says_it_is() {
        // 400 graph + 100 offload + 100 audit + 200 mcp + 200 mcp_health
        // + 200 offload_server + 60 sandbox + 100 plugin + 100 delegation
        // + 30 settings
        const PER_KIND_WINDOWS: usize = 1_490;
        assert_eq!(
            TOTAL_CAPACITY,
            PER_KIND_WINDOWS + INJECTION_FLAG_TOTAL_CAP,
            "a per-kind retention cap changed — intended?"
        );
        // `injection_flag` reaches the sum through its lane aggregate and NOT
        // through a kind cap: it has none, so `kind_cap` answers for it exactly
        // as it answers for a kind written by a newer build.
        assert_eq!(kind_cap(ActivityKind::InjectionFlag.as_str()), GRAPH_CAP);
    }

    /// Compaction must leave room to append (#48, H-9).
    ///
    /// A rewrite resets `file_lines` to `ring.len()`, so if the trigger sat at
    /// or below the store's total capacity, a saturated store would re-read and
    /// rewrite the whole file on **every** record. The old literal `1000` was
    /// exactly the then-total; adding lanes without this relation would have
    /// walked the write path onto that cliff silently. (The constant relation
    /// itself is a `const _: () = assert!(…)` up top — a build failure, not a
    /// test failure. What this adds is the observed behaviour: a store that has
    /// actually compacted has room to append again.)
    #[test]
    fn compaction_leaves_room_to_append() {
        let store = temp_store("compact-headroom");
        for i in 0..(FILE_COMPACT_LINES + 5) {
            store.record(rec(&format!("g{i}")));
        }
        let inner = store.inner.lock().expect("lock");
        assert_eq!(inner.ring.len(), GRAPH_CAP);
        assert!(
            inner.file_lines + FILE_COMPACT_SLACK <= FILE_COMPACT_LINES,
            "after compaction the file must have room for {FILE_COMPACT_SLACK} appends, had {}",
            FILE_COMPACT_LINES - inner.file_lines
        );
        drop(inner);
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn delete_preserves_foreign_appends() {
        // Simulate an `--offload-mcp` child appending a line this process's
        // ring has never seen: a delete-rewrite must fold it in, not clobber
        // it.
        let store = temp_store("foreign");
        store.record(rec("mine"));
        let mine = store.snapshot_since(0)[0].id;
        let mut foreign = rec("from-the-child");
        foreign.entry.id = mine + 7; // a distinct, non-zero foreign id
        let line = serde_json::to_string(&foreign).unwrap();
        {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(&store.path)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }

        assert!(store.delete(mine));
        let after = ActivityStore::new(store.path.clone()).snapshot_since(0);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].target, "from-the-child");
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn load_repairs_duplicate_ids() {
        // Two processes can (rarely) mint the same id; on load the duplicate
        // must be re-keyed so delete-by-id can't remove both.
        let store = temp_store("dupe");
        let mut a = rec("first");
        let mut b = rec("second");
        a.entry.id = 42_000_000_000_000_000;
        b.entry.id = a.entry.id;
        let path = store.path.clone();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = serde_json::to_string(&a).unwrap();
        text.push('\n');
        text.push_str(&serde_json::to_string(&b).unwrap());
        text.push('\n');
        fs::write(&path, text).unwrap();

        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 2);
        assert_ne!(snap[0].id, snap[1].id);
        let _ = fs::remove_file(&path);
    }

    /// V37 C6: `mcp_health` has its own retention lane. The rows that explain a
    /// server going down are written exactly when that server stops producing
    /// `mcp` rows, so sharing MCP_CAP with call traffic would mean the OTHER,
    /// still-busy servers evict the only record of the quiet one.
    #[test]
    fn mcp_health_rows_keep_their_own_window() {
        let store = temp_store("mcp-health-lane");
        store.record(rec_kind(ActivityKind::McpHealth, "ddg went unhealthy"));
        for i in 0..(MCP_CAP + 50) {
            store.record(rec_kind(ActivityKind::Mcp, &format!("call {i}")));
        }
        for i in 0..(GRAPH_CAP + 50) {
            store.record(rec(&format!("g{i}")));
        }
        let snap = store.snapshot_since(0);
        let rows: Vec<_> = snap.iter().filter(|e| e.kind == "mcp_health").collect();
        assert_eq!(rows.len(), 1, "the health row was evicted by call traffic");
        assert_eq!(rows[0].target, "ddg went unhealthy");
        let _ = fs::remove_file(&store.path);
    }

    /// …and is itself bounded: a server flapping every cadence writes two rows a
    /// minute forever, and must not be able to grow the store without limit.
    #[test]
    fn mcp_health_rows_are_capped_at_their_own_window() {
        let store = temp_store("mcp-health-cap");
        for i in 0..(MCP_HEALTH_CAP + 40) {
            store.record(rec_kind(ActivityKind::McpHealth, &format!("flap {i}")));
        }
        let snap = store.snapshot_since(0);
        assert_eq!(
            snap.iter().filter(|e| e.kind == "mcp_health").count(),
            MCP_HEALTH_CAP
        );
        assert_eq!(
            snap.iter()
                .find(|e| e.kind == "mcp_health")
                .map(|e| e.target.as_str()),
            Some(format!("flap {}", MCP_HEALTH_CAP + 39).as_str())
        );
        let _ = fs::remove_file(&store.path);
    }

    /// V37 C7: `server`/`category` are REQUIRED at the writer and defaulted at
    /// the reader, and those are different questions. Every row on disk predates
    /// the columns, so a load that failed on them would drop a user's whole
    /// activity history the first time they upgraded.
    #[test]
    fn a_pre_v37_row_without_server_or_category_still_loads() {
        let store = temp_store("pre-v37-row");
        let path = store.path.clone();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Written by hand rather than by serializing an entry: the point is a
        // line that does NOT have the keys, which today's writer cannot produce.
        let line = r#"{"id":7,"ts_ms":1000,"kind":"mcp","root":"/p","source":"claude","tool":"ddg__search","target":"rust","chars":12,"ms":3,"ok":true,"tab":"unattributed","session":null,"request":"{}","response":"ok"}"#;
        fs::write(&path, format!("{line}
")).unwrap();

        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 1, "the pre-V37 row failed to parse");
        assert_eq!(snap[0].tool, "ddg__search");
        // Absent, not empty — and NOT back-filled by splitting `ddg__search`.
        assert_eq!(snap[0].server, None);
        assert_eq!(snap[0].category, None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn snapshot_since_filters_older_entries() {
        let store = temp_store("since");
        let mut old = rec("old");
        old.entry.ts_ms = 1000;
        let mut new = rec("new");
        new.entry.ts_ms = 2000;
        store.record(old);
        store.record(new);
        let fresh = store.snapshot_since(1000);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].target, "new");
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn payloads_are_truncated_with_marker() {
        let store = temp_store("truncate");
        let mut r = rec("big");
        r.response = "x".repeat(RESPONSE_CAP_CHARS + 500);
        store.record(r);
        let id = store.snapshot_since(0)[0].id;
        let full = store.detail(id).expect("detail");
        assert!(full.response.contains("… [truncated 500 chars]"));
        assert!(full.response.chars().count() < RESPONSE_CAP_CHARS + 100);
        let _ = fs::remove_file(&store.path);
    }

    // ── Row status (#48, M-24; V42: ported from `src/lib/activity.test.ts`) ──
    //
    // The finding these came from: `unscreened`, the detector flags,
    // `MemoryQuarantine` and `LatchOverride` all collapsed into ONE red chip, so
    // "we did not look at all of it" read as "we blocked something" — the
    // opposite of the truth — and a latch override the USER applied to hand
    // capability back read as containment firing.
    //
    // They pin the DISTINCTIONS rather than the current words: what must not
    // regress is that no two of these facts share a status, and that the only
    // status meaning "we stopped it" is reached by the screens that actually
    // did. One case per test in the deleted `describe`s, plus the two the move
    // itself makes possible: the vocabulary is now checked against
    // `Screen::is_denial` and against the frontend union it has to keep feeding.

    /// One row as the classifier sees it — `kind`, `source`, `tool` and `ok` are
    /// the only columns it reads. Built through [`ActivityEntry::new`], so every
    /// case below also exercises the classification at the recording site.
    fn row(kind: ActivityKind, source: &str, tool: &str, ok: bool) -> ActivityEntry {
        ActivityEntry::new(
            kind,
            1,
            "r".into(),
            source.into(),
            tool.into(),
            "t".into(),
            0,
            0,
            ok,
            Attribution::Unattributed,
            None,
            None,
            None,
        )
    }

    fn status(kind: ActivityKind, source: &str, tool: &str, ok: bool) -> RowStatus {
        row(kind, source, tool, ok).status
    }

    /// One `injection_flag` row for `screen`, with the `ok` `record_flag` would
    /// publish for it.
    fn flag(screen: Screen) -> RowStatus {
        status(
            ActivityKind::InjectionFlag,
            screen.as_str(),
            "WebFetch",
            !screen.is_denial(),
        )
    }

    #[test]
    fn every_containment_screen_gets_its_own_status_and_no_two_collapse() {
        // The four denials share `denied`, which is correct — they all stopped a
        // call. The five that stopped nothing must each differ from that AND
        // from one another.
        for screen in [
            Screen::Ssrf,
            Screen::Budget,
            Screen::Canary,
            Screen::LatchRefusal,
        ] {
            assert_eq!(flag(screen), RowStatus::Denied, "{screen:?} stopped a call");
        }
        let non_denials = [
            flag(Screen::Signature),
            flag(Screen::Unscreened),
            flag(Screen::MemoryQuarantine),
            flag(Screen::LatchOverride),
            flag(Screen::LatchBeacon),
        ];
        let distinct: std::collections::BTreeSet<&str> =
            non_denials.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            distinct.len(),
            non_denials.len(),
            "five screens that denied nothing, five different words: {non_denials:?}"
        );
        assert!(
            !non_denials.contains(&RowStatus::Denied),
            "nothing that stopped nothing may wear the word that means we stopped it"
        );
    }

    /// **The invariant the frontend copy could only restate.**
    ///
    /// `denied` is this app's one "we blocked something", and the screens that
    /// earn it are exactly the ones [`Screen::is_denial`] answers `true` for.
    /// The classifier names them individually (so a new screen must choose a
    /// word rather than inherit one) — this is what says the two lists agree,
    /// rather than trusting that they happen to.
    #[test]
    fn every_denial_screen_and_only_a_denial_screen_reads_as_denied() {
        for &screen in Screen::ALL {
            assert_eq!(
                flag(screen) == RowStatus::Denied,
                screen.is_denial(),
                "{screen:?}: `is_denial` says {} but the row reads `{}`",
                screen.is_denial(),
                flag(screen).as_str()
            );
        }
    }

    #[test]
    fn an_unscreened_result_is_never_a_denial_and_never_clean() {
        // The whole finding in one assertion: an absent verdict is neither a
        // verdict of absence nor an alarm.
        let s = flag(Screen::Unscreened);
        assert_eq!(s, RowStatus::Unscreened);
        assert_ne!(s, RowStatus::Denied);
        assert_ne!(s, RowStatus::Ok);
    }

    #[test]
    fn a_user_latch_override_reads_as_a_grant_not_as_containment_firing() {
        assert_eq!(flag(Screen::LatchOverride), RowStatus::Granted);
        assert_eq!(flag(Screen::ContaminationCleared), RowStatus::Granted);
        // A grant and a block must not share a word — a release that reads as a
        // refusal is the inverted half of the same defect.
        assert_ne!(flag(Screen::LatchOverride), RowStatus::Denied);
    }

    #[test]
    fn a_held_memory_write_reads_as_held_not_as_a_refusal() {
        assert_eq!(flag(Screen::MemoryQuarantine), RowStatus::Held);
    }

    #[test]
    fn a_rejected_updater_bundle_is_not_a_blocked_call() {
        // `updater` is the one source written outside `record_flag`: its `ok` is
        // the bundle OUTCOME, not `Screen::is_denial`. Reading `!ok` as "denied"
        // there reported a refused rules bundle as a blocked tool call.
        let src = Screen::Updater.as_str();
        assert_eq!(
            status(ActivityKind::InjectionFlag, src, "rules", true),
            RowStatus::Update
        );
        assert_eq!(
            status(ActivityKind::InjectionFlag, src, "rules", false),
            RowStatus::Rejected
        );
        assert_ne!(
            status(ActivityKind::InjectionFlag, src, "rules", false),
            RowStatus::Denied
        );
    }

    #[test]
    fn an_unknown_screen_gets_no_category_rather_than_a_borrowed_one() {
        // A screen a WRITER declares and this reader does not. Delivered ⇒ we
        // have no word for it; refused ⇒ `Screen::is_denial` is a claim we can
        // still make.
        let unknown = "some_future_screen";
        assert_eq!(
            status(ActivityKind::InjectionFlag, unknown, "WebFetch", true),
            RowStatus::Recorded
        );
        assert_eq!(
            status(ActivityKind::InjectionFlag, unknown, "WebFetch", false),
            RowStatus::Denied
        );
    }

    #[test]
    fn the_three_plain_call_outcomes_are_intact() {
        assert_eq!(
            status(ActivityKind::Graph, "offload", "graph_outline", true),
            RowStatus::Ok
        );
        assert_eq!(
            status(ActivityKind::Graph, "offload", "graph_outline", false),
            RowStatus::Failed
        );
        // Telemetry channels record ok:false to mean "this signal fired".
        for source in CANARY_SOURCES {
            assert_eq!(
                status(ActivityKind::Graph, source, "graph_outline", false),
                RowStatus::Signal,
                "{source} is a telemetry channel, not a broken call"
            );
        }
    }

    // ── V37 C6: mcp_health rows ──────────────────────────────────────────

    #[test]
    fn the_health_lane_gives_the_down_transitions_and_the_recovery_different_words() {
        assert_eq!(
            status(ActivityKind::McpHealth, "probe", "unhealthy", false),
            RowStatus::Unhealthy
        );
        assert_eq!(
            status(ActivityKind::McpHealth, "connect", "connect_failed", false),
            RowStatus::Unhealthy
        );
        assert_eq!(
            status(ActivityKind::McpHealth, "probe", "healthy", true),
            RowStatus::Recovered
        );
    }

    #[test]
    fn the_health_lane_never_borrows_the_offload_server_vocabulary() {
        // `down`/`ready`/`stopped` are written about a process cImp owns and
        // stopped; an MCP server is somebody else's.
        let seen = [
            status(ActivityKind::McpHealth, "probe", "unhealthy", false),
            status(ActivityKind::McpHealth, "probe", "healthy", true),
        ];
        for borrowed in [RowStatus::Down, RowStatus::Ready, RowStatus::Stopped] {
            assert!(!seen.contains(&borrowed), "{borrowed:?} belongs to the offload lane");
        }
    }

    #[test]
    fn an_unknown_health_transition_falls_back_on_ok() {
        assert_eq!(
            status(ActivityKind::McpHealth, "probe", "quarantined", false),
            RowStatus::Unhealthy
        );
        assert_eq!(
            status(ActivityKind::McpHealth, "probe", "quarantined", true),
            RowStatus::Recovered
        );
    }

    #[test]
    fn an_ordinary_mcp_call_row_is_not_a_health_row() {
        // The two kinds share a server but not a lane: a failed call is a failed
        // call, not a server going down (that is what the flap guard is for).
        assert_eq!(
            status(ActivityKind::Mcp, "claude", "ddg__search", false),
            RowStatus::Failed
        );
    }

    // ── V37 C9: tools withheld by description screening ──────────────────

    #[test]
    fn a_withheld_tool_is_its_own_status_not_a_failed_call() {
        // These rows land in the `mcp` lane with `ok: false`; with no branch for
        // them the classifier fell through to `failed`, whose sentence is "Call
        // failed" — a claim about a call that was never made.
        let s = status(
            ActivityKind::Mcp,
            crate::offload::mcp_host::SCREEN_DROP_SOURCE,
            "exfiltrate",
            false,
        );
        assert_eq!(s, RowStatus::Withheld);
        assert_ne!(s, RowStatus::Failed);
        // `flagged` would be worse, not better: that word's whole promise is
        // "nothing was blocked", and this is the one place in cImp where
        // detection actually REMOVES something.
        assert_ne!(s, RowStatus::Flagged);
    }

    #[test]
    fn the_withheld_branch_keys_on_the_exact_wire_source_not_on_the_kind() {
        // A near-miss source is not a screening row …
        assert_eq!(
            status(ActivityKind::Mcp, "screening", "x", false),
            RowStatus::Failed
        );
        // … and the source alone does not hijack another lane.
        assert_eq!(
            status(
                ActivityKind::Graph,
                crate::offload::mcp_host::SCREEN_DROP_SOURCE,
                "graph_outline",
                false
            ),
            RowStatus::Failed
        );
    }

    // ── offload_server lifecycle rows ────────────────────────────────────

    #[test]
    fn every_lifecycle_transition_gets_its_own_status() {
        let seen = [
            status(ActivityKind::OffloadServer, "big-local", "start", true),
            status(ActivityKind::OffloadServer, "big-local", "ready", true),
            status(ActivityKind::OffloadServer, "big-local", "stop", true),
            status(ActivityKind::OffloadServer, "big-local", "fail", false),
        ];
        assert_eq!(
            seen,
            [
                RowStatus::Started,
                RowStatus::Ready,
                RowStatus::Stopped,
                RowStatus::Down
            ]
        );
    }

    #[test]
    fn a_deliberate_stop_is_never_read_as_a_failure() {
        // The backend records a stop as ok:true precisely so this holds; pinned
        // here because the classifier must not re-derive it from `ok` either
        // way. Both a healthy start and a stop are ok:true, so anything keyed on
        // `ok` would render "the server is up" and "the server is gone" as one
        // word.
        let stop = status(ActivityKind::OffloadServer, "big-local", "stop", true);
        assert_eq!(stop, RowStatus::Stopped);
        assert_ne!(stop, RowStatus::Down);
        assert_ne!(stop, RowStatus::Failed);
        assert_ne!(
            status(ActivityKind::OffloadServer, "big-local", "start", true),
            stop
        );
    }

    #[test]
    fn a_server_failure_is_not_a_failed_tool_call() {
        // `failed` is a call that errored; `down` is a backend that is not
        // running. Sharing a word would put a crashed llama-server in the same
        // bucket as a graph query that threw.
        assert_eq!(
            status(ActivityKind::OffloadServer, "big-local", "fail", false),
            RowStatus::Down
        );
        assert_eq!(
            status(ActivityKind::Graph, "offload", "graph_outline", false),
            RowStatus::Failed
        );
    }

    #[test]
    fn an_unknown_lifecycle_transition_degrades_without_inventing_a_claim() {
        assert_eq!(
            status(ActivityKind::OffloadServer, "big-local", "paused", true),
            RowStatus::Ok
        );
        assert_eq!(
            status(ActivityKind::OffloadServer, "big-local", "paused", false),
            RowStatus::Down
        );
    }

    #[test]
    fn an_offload_task_row_is_not_a_lifecycle_row() {
        // The two kinds are one underscore apart and both carry a backend name
        // in `source`; a prefix match instead of an equality check would swallow
        // the task feed whole.
        assert_eq!(
            status(ActivityKind::Offload, "big-local", "offload_task", true),
            RowStatus::Ok
        );
    }

    // ── sandbox rows (V33 Phase A) ───────────────────────────────────────

    #[test]
    fn both_negative_sandbox_states_read_as_unsandboxed() {
        // Locked decision 17 keeps "off (user choice)" and "unavailable" DISTINCT
        // states; `ok` is which of the two it was, so it cannot also carry the
        // verb, and the distinction the chip does not show lives in `target`.
        let chosen = status(ActivityKind::Sandbox, "run_command", "unsandboxed", true);
        let unavailable = status(ActivityKind::Sandbox, "run_command", "unsandboxed", false);
        assert_eq!(chosen, RowStatus::Unsandboxed);
        assert_eq!(unavailable, RowStatus::Unsandboxed);
        // The command ran fine in both cases: this must never wear the words that
        // mean "we stopped something" or "the call errored".
        assert_ne!(chosen, RowStatus::Denied);
        assert_ne!(unavailable, RowStatus::Failed);
    }

    #[test]
    fn a_grant_event_is_not_an_unsandboxed_run() {
        assert_eq!(
            status(ActivityKind::Sandbox, "run_command", "grant", true),
            RowStatus::Ok
        );
        assert_eq!(
            status(ActivityKind::Sandbox, "run_command", "grant", false),
            RowStatus::Failed
        );
    }

    #[test]
    fn a_sandboxed_run_reads_as_ordinary_traffic_not_as_an_alarm() {
        // The confirmation row exists to answer "is this actually sandboxed?" —
        // an empty lane used to mean either "everything was" or "nothing ran".
        // Answering it must not cost the lane its signal-to-noise.
        assert_eq!(
            status(ActivityKind::Sandbox, "run_command", "sandboxed", true),
            RowStatus::Ok
        );
    }

    #[test]
    fn a_suspected_boundary_hit_gets_its_own_word_neither_denied_nor_failed() {
        let hit = status(ActivityKind::Sandbox, "run_command", "denied", false);
        assert_eq!(hit, RowStatus::Boundary);
        // `denied` is this app's one "we stopped it", and it is filled red. cImp
        // cannot see the OS's ACL decision — the backend words this row as a
        // heuristic, so the chip must not assert more than the row does.
        assert_ne!(hit, RowStatus::Denied);
        // `failed` is wrong the other way: the tool call itself returned output.
        assert_ne!(hit, RowStatus::Failed);
    }

    #[test]
    fn every_sandbox_row_type_stays_visibly_distinct() {
        // One lane, four row types. If two of them ever render as the same word,
        // the lane stops answering the question it was added to answer.
        let words = [
            status(ActivityKind::Sandbox, "run_command", "unsandboxed", true),
            status(ActivityKind::Sandbox, "run_command", "sandboxed", true),
            status(ActivityKind::Sandbox, "run_command", "denied", false),
            status(ActivityKind::Sandbox, "run_command", "grant", false),
        ];
        let distinct: std::collections::BTreeSet<&str> = words.iter().map(|w| w.as_str()).collect();
        assert_eq!(distinct.len(), words.len(), "{words:?}");
    }

    // ── plugin discovery rows (V38 Phase A) ──────────────────────────────

    #[test]
    fn a_rejected_manifest_is_a_plain_failure_never_a_blocked_call() {
        // This lane deliberately adds NO new word: a definition either loaded or
        // it did not. What it must not do is fall into any of the words that
        // carry a security claim — a rejected manifest is a malformed FILE.
        let rejected = status(ActivityKind::Plugin, "acme@1.0.0", "rejected", false);
        assert_eq!(rejected, RowStatus::Failed);
        for security_word in [RowStatus::Denied, RowStatus::Flagged, RowStatus::Boundary] {
            assert_ne!(rejected, security_word);
        }
        assert_eq!(
            status(ActivityKind::Plugin, "acme@1.0.0", "conflict", false),
            RowStatus::Failed
        );
    }

    #[test]
    fn the_plugin_scan_summary_reports_the_folder_at_a_glance() {
        // The backend sets `ok` on the summary to the FOLDER's health, so a
        // clean folder is one green row and a folder with a rejected plugin is
        // not.
        assert_eq!(
            status(ActivityKind::Plugin, "plugins", "rescan", true),
            RowStatus::Ok
        );
        assert_eq!(
            status(ActivityKind::Plugin, "plugins", "rescan", false),
            RowStatus::Failed
        );
    }

    // ── delegation rows (V39, locked decision 14) ────────────────────────

    #[test]
    fn the_three_non_outcome_delegation_transitions_get_their_own_word() {
        // The lane where `ok` carries the least: a `start` row is ok:true before
        // anything has happened, a `takeover` is ok:false because the user chose
        // to end it, and a `role_moved` is not a call at all.
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "start", true),
            RowStatus::Driving
        );
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "takeover", false),
            RowStatus::Takeover
        );
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "role_moved", true),
            RowStatus::Moved
        );
        // The user reclaiming their own tab is deliberate, and the worker kept
        // running: never a failure.
        assert_ne!(
            status(ActivityKind::Delegation, "some-harness", "takeover", false),
            RowStatus::Failed
        );
        // Both a start and a completed reply are ok:true; anything keyed on `ok`
        // would return one word for the two.
        assert_ne!(
            status(ActivityKind::Delegation, "some-harness", "start", true),
            status(ActivityKind::Delegation, "some-harness", "done", true)
        );
    }

    #[test]
    fn the_real_delegation_outcomes_stay_ok_or_failed_rather_than_inventing_synonyms() {
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "done", true),
            RowStatus::Ok
        );
        // A completed turn whose text was not substantive is a `done` that
        // FAILED (locked decision 13) — the worker really did run. `driver_gone`
        // (V39 review L-7) and the reserved `cancelled` have no word of their own
        // on purpose: `failed` plus the row's own reason says it, and a synonym
        // would dilute the three words that DO mean something.
        for tool in [
            "done",
            "refused",
            "timeout",
            "worker_exited",
            "driver_gone",
            "cancelled",
        ] {
            assert_eq!(
                status(ActivityKind::Delegation, "some-harness", tool, false),
                RowStatus::Failed,
                "`{tool}` is a delegation that failed"
            );
        }
        // An unknown transition degrades without inventing a claim.
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "resumed", true),
            RowStatus::Ok
        );
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "resumed", false),
            RowStatus::Failed
        );
    }

    #[test]
    fn the_driver_harness_is_not_a_telemetry_channel() {
        // `source` on a delegation row is a harness id, not a canary channel — a
        // `refused` row must read as a failed delegation, not as a signal.
        assert_eq!(
            status(ActivityKind::Delegation, "some-harness", "refused", false),
            RowStatus::Failed
        );
    }

    // ── the status is derived, on the wire AND on disk ───────────────────

    #[test]
    fn a_row_written_before_the_status_column_gets_one_at_read() {
        // Every row in an existing `tool-activity.jsonl` was written without the
        // column. The classifier reads only fields those rows already carry, so
        // deriving at read is behaviour-preserving for files already on disk —
        // and it is what keeps an old row rendering in today's vocabulary.
        let store = temp_store("legacy-status");
        fs::create_dir_all(store.path.parent().expect("temp dir")).expect("mkdir");
        fs::write(
            &store.path,
            "{\"id\":7,\"ts_ms\":1,\"kind\":\"injection_flag\",\"root\":\"r\",\
             \"source\":\"unscreened\",\"tool\":\"WebFetch\",\"target\":\"x\",\
             \"chars\":3,\"ms\":4,\"ok\":true}\n",
        )
        .expect("seed");
        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].status,
            RowStatus::Unscreened,
            "a pre-column row is classified at read, not left at the placeholder"
        );
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn a_stored_status_is_never_read_back() {
        // The word is serialized so a JSONL line reads on its own, but the
        // stored copy is not evidence: a row written by an older build carries
        // that build's reading, and a hand-edited file carries whatever was
        // typed. The classifier is the authority on both sides of the disk.
        let store = temp_store("stale-status");
        fs::create_dir_all(store.path.parent().expect("temp dir")).expect("mkdir");
        fs::write(
            &store.path,
            "{\"id\":8,\"ts_ms\":1,\"kind\":\"graph\",\"root\":\"r\",\"source\":\"claude\",\
             \"tool\":\"graph_outline\",\"target\":\"x\",\"chars\":3,\"ms\":4,\"ok\":true,\
             \"status\":\"denied\"}\n",
        )
        .expect("seed");
        let snap = store.snapshot_since(0);
        assert_eq!(snap.len(), 1);
        assert_eq!(
            snap[0].status,
            RowStatus::Ok,
            "a stored word must not be able to paint a successful call as a denial"
        );
        let _ = fs::remove_file(&store.path);
    }

    #[test]
    fn the_status_a_row_carries_is_the_one_it_is_recorded_with() {
        // `record` re-classifies at the funnel rather than trusting its caller,
        // so a recorder that edits a column after `ActivityEntry::new` (the lane
        // tests here do exactly that) still stores the word for the row it
        // actually wrote.
        let store = temp_store("record-status");
        let mut r = rec_kind(ActivityKind::InjectionFlag, "held note");
        r.entry.source = Screen::MemoryQuarantine.as_str().to_string();
        store.record(r);
        assert_eq!(store.snapshot_since(0)[0].status, RowStatus::Held);
        let _ = fs::remove_file(&store.path);
    }

    /// **Every word this side can publish is a word the frontend can render**,
    /// and nothing more.
    ///
    /// The TypeScript union is the rendering vocabulary: `STATUS_TITLE` is keyed
    /// by it (a missing key is a tooltipless chip) and `StatusChip`'s scoped
    /// style is classed with it (a missing rule is an unstyled one — the
    /// F-V37-1 defect that shipped `unhealthy`/`recovered` styleless). Both of
    /// those guards live in `activity.test.ts` and iterate the union, so the
    /// union is what has to match what this side emits — a drift `cargo` and
    /// `vitest` are each individually blind to.
    ///
    /// Newline-agnostic: CI checks this tree out with CRLF.
    #[test]
    fn the_status_vocabulary_matches_the_frontends() {
        let ts = include_str!("../../src/lib/activity.ts").replace('\r', "");
        let decl = "export type RowStatus =";
        let at = ts.find(decl).expect(
            "`export type RowStatus =` is gone from src/lib/activity.ts — the union moved or was \
             renamed, and this guard is now watching nothing. Point it at the new name.",
        );
        let body_len = ts[at..].find(';').expect("the union is never terminated");
        let mut declared: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for line in ts[at..at + body_len].lines() {
            if let Some(word) = line
                .trim()
                .strip_prefix("| '")
                .and_then(|rest| rest.strip_suffix('\''))
            {
                declared.insert(word);
            }
        }
        assert!(
            !declared.is_empty(),
            "parsed no words out of the `RowStatus` union"
        );
        let ours: std::collections::BTreeSet<&str> =
            RowStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            ours, declared,
            "the Rust and TypeScript `RowStatus` vocabularies have drifted — a word only Rust \
             knows renders as an unstyled, tooltipless chip; a word only TypeScript knows is a \
             sentence and a colour nothing can reach"
        );
    }
}
