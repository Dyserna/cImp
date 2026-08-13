//! V33 Phase F: the `cimp --checkpoint-beacon` Claude Code `PreToolUse` shim —
//! the seam that makes a Workbench checkpoint fire immediately before Claude's
//! OWN filesystem-mutating tools (`Edit` / `Write` / `MultiEdit` / `Bash`).
//!
//! # Why this exists
//!
//! The Workbench Timeline already checkpoints on every prompt (V13 Phase C) and
//! on filesystem bursts. Neither answers the question an incident actually
//! asks — *which tool call broke this, and what did the tree look like just
//! before it*. A prompt checkpoint covers a whole turn, which by the time it
//! matters contains a dozen edits; a burst checkpoint fires after the damage, by
//! construction. This shim POSTs to the loopback's
//! `/workbench/tool_checkpoint`, which takes a checkpoint carrying a `Source:`
//! trailer naming the exact call (`claude:Bash`), so the Timeline can attribute
//! the damage to it and rewind to just before it.
//!
//! # Report-only, and structurally incapable of denying
//!
//! Modelled on [`crate::taint_beacon`], and for the same locked reason (V32
//! decision 14, carried into V33 C8): *"Hooks never deny; a hook/loopback
//! failure is silently fail-open."* A `PreToolUse` hook denies only by SAYING
//! so — exit code **2**, or exit 0 with `hookSpecificOutput.permissionDecision:
//! "deny"` on stdout. This shim writes **nothing** to either stream and returns
//! normally on every path, so a dead app, a rotated token or a 401 all end as
//! "the call proceeds, unrecorded". There is no branch here that can produce a
//! denial.
//!
//! # ⚠ THIS SHIM WAITS — and `taint_beacon`, the file it was copied from, must
//! not. Do not "restore consistency" between them.
//!
//! **Amendment 2026-08-13 (user decision), C8 in
//! `docs/IMPL-PLAN-V33-sandboxing.md`.** As first written this shim inherited
//! `taint_beacon`'s discipline verbatim — dispatch the POST, never read the
//! reply, so no app-controlled duration is ever on a tool call's path. For a
//! *sensor* that is right: a lost beacon understates taint for one call, and
//! nothing about a beacon has an ordering requirement.
//!
//! For a *pre-tool checkpoint* it silently broke the feature's central claim.
//! Because the shim did not wait, the app ran the snapshot **concurrently** with
//! Claude's tool execution, and on a large work tree a `git add -A` can outlast
//! a small `Edit` — so that call's own change lands inside the checkpoint that
//! exists to precede it. A checkpoint that sometimes contains the change it
//! claims to predate is worse than no checkpoint: it silently misleads a
//! restore. That is the sibling of the dedup hazard C8 already guards (never
//! claim a checkpoint you did not create).
//!
//! So this shim reads its reply, with a [`REPLY_TIMEOUT`] of 2 s.
//!
//! **What the deadline buys:** inside it, "the checkpoint precedes the tool
//! call" is *exact* on all three Phase F seams rather than exact on two and
//! best-effort here. Claude does not start the tool until this process exits,
//! and this process does not exit until the app has answered that the snapshot
//! settled.
//!
//! **What exceeding it costs:** the app abandons the snapshot unwritten
//! (`loopback::TOOL_CHECKPOINT_BUDGET`, deliberately set *under* this timeout so
//! the app's answer is the one that decides), so that call gets **no checkpoint
//! and no row claiming to be one**, and the miss is surfaced as its own Activity
//! event (`workbench` / `checkpoint_missed`). The tool then runs unprotected —
//! which is the honest state of the world, and is what the previous checkpoint
//! plus the Timeline's prompt-level granularity already covered.
//!
//! **What it does NOT cost: the tool never waits on a broken app.** Every
//! failure path here is unchanged and instant — no discovery file, a refused
//! connect, a 401, a peer that closes without answering. Only a *live* app
//! actually taking a long time can spend the 2 s, and 5 s of harness-side
//! ceiling still sits above that (`tabs::config`'s `"timeout": 5`). The
//! undocumented "what does Claude do with a TIMED-OUT hook" question that
//! `taint_beacon` is built around is therefore still not load-bearing: nothing
//! here approaches the ceiling, and if it somehow did, the worst case is the
//! same fail-open the exit-code contract gives.
//!
//! # Cost of the wait, and why it is affordable
//!
//! Every `Edit`/`Write`/`MultiEdit`/`Bash` in a Claude tab now waits for the
//! app's answer. **The throttle, not the dedup, is what makes that nearly free:**
//! the app admits at most one snapshot per `checkpoint_min_gap_s` per
//! `(project root, tab)`, and a throttled call returns without touching git at
//! all (`workbench::CheckpointScheduler::spawn_if_due` returns `None` before
//! spawning anything). A dedup hit is *not* free — it still pays the whole
//! `git add -A` before discovering the tree is unchanged — so the affordability
//! argument rests on the gate, and is pinned by
//! `workbench::tests::a_blown_pre_tool_budget_reports_itself_and_the_throttle_costs_no_git`.
//!
//! The contract-drift report below is still fire-and-forget: it has nothing to
//! order against, and making it wait would add a second app-controlled duration
//! to the one path that already knows the harness is misbehaving.
//!
//! # Identity
//!
//! The hook payload carries `session_id` and `cwd` but no cImp TAB id, and the
//! checkpoint's whole purpose is to be attributable to one tab — so `--tab <id>`
//! is baked into the hook command at spawn (`tabs/config.rs`), exactly as
//! `--taint-beacon`'s is. Without it there is nothing to attribute, and the shim
//! returns without POSTing rather than writing an unattributed checkpoint.
//!
//! # Cost per call
//!
//! One process spawn plus one loopback POST, and ONLY on the four mutating tools
//! the matcher names — `Read`/`Grep`/`WebFetch` pay nothing. A burst of edits
//! inside one min-gap window costs four POSTs and one `git` round trip.
//!
//! Worst-case wall clock, which now matters because the shim waits: up to 1.2 s
//! of cold discovery probing (`MAX_DISCOVERY_PROBES × DISCOVERY_PROBE_TIMEOUT`,
//! memoized after the first resolution), 80 ms connect, 80 ms write, 2 s reply
//! — 3.36 s against the hook entry's 5 s ceiling, asserted by
//! [`tests::the_shim_waits_longer_than_the_app_takes_to_give_up`].

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::context_hook::{missing_fields, resolve_cwd, tab_arg};

/// The harness vocabulary this shim reports under — the left half of the
/// `harness:tool_name` source value, and what tells the route which tool table
/// to resolve the name against.
const CONSUMER: &str = "claude";

/// The connect and write budget, applied to each separately. Deliberately
/// identical to [`crate::taint_beacon`]'s: on loopback a live app accepts into
/// the backlog immediately and a dead one is refused immediately, so this bound
/// covers only a wedged app — the case where waiting would be worst. It stays
/// sub-100 ms even though the shim now waits for a reply, because *reaching* the
/// app is not the part that can legitimately take time.
const DISPATCH_TIMEOUT: Duration = Duration::from_millis(80);

/// **How long this shim waits for the app to finish the snapshot** — the
/// 2026-08-13 amendment's deadline. See the module doc for what it buys and
/// what exceeding it costs.
///
/// It is the *outer* of two bounds and is not meant to be the binding one: the
/// app abandons an unfinished snapshot at `loopback::TOOL_CHECKPOINT_BUDGET`
/// (1800 ms) and answers, so in every case where the app is alive this timeout
/// is a backstop for a wedged listener rather than the mechanism. The same
/// relationship `taint_beacon` has with the harness's 5 s hook ceiling, one
/// layer in.
///
/// Applied per read syscall, which is what a blocking socket offers. That is
/// exact enough here: the reply is two booleans written in one go and followed
/// by a close, so the realistic shapes are "one read, then EOF" and "the first
/// read blocks for the whole timeout".
const REPLY_TIMEOUT: Duration = Duration::from_millis(2000);

/// Whether [`dispatch`] waits for the app's answer.
///
/// An enum rather than a `bool` parameter, and **local to this module rather
/// than shared with [`crate::taint_beacon`]'s dispatcher** — the two shims'
/// contracts are opposites now (that one must never wait; this one must), and a
/// shared helper with a `read_reply` flag is precisely the coupling that would
/// let a future edit flip one of them by touching the other.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reply {
    /// Write and return. For the contract-drift report, which has nothing to
    /// order against.
    Ignore,
    /// Read until the peer closes, or [`REPLY_TIMEOUT`] elapses. The bytes are
    /// discarded — what matters is *that* the app answered, which is what makes
    /// "the snapshot finished before this process exited" true.
    Await,
}

pub fn run() {
    // The tab id is baked into argv at spawn. No id ⇒ nothing to attribute;
    // return before touching stdin or the network.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(tab) = tab_arg(&args) else {
        return;
    };

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let v: serde_json::Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(_) => return,
    };
    let tool_name = str_field(&v, "tool_name");
    let session_id = str_field(&v, "session_id");
    let cwd_raw = str_field(&v, "cwd");

    // Payload-shape drift, reported BEFORE any early return (the discipline
    // every sibling shim follows), through this module's OWN non-waiting
    // dispatcher. `cwd` is the one that would silently break the feature: the
    // checkpoint would be taken against `.` — this process's cwd, which Claude
    // sets to the project dir, so it usually still works and would hide the
    // drift rather than surface it.
    let missing = missing_fields(&[
        ("tool_name", !tool_name.is_empty()),
        ("session_id", !session_id.is_empty()),
        ("cwd", !cwd_raw.is_empty()),
    ]);
    if !missing.is_empty() {
        dispatch(
            "/activity/contract_drift",
            &serde_json::json!({
                "shim": "checkpoint_beacon",
                "missing": missing,
                "session_id": session_id,
            })
            .to_string(),
            Reply::Ignore,
        );
    }

    // An empty tool name is the one field this shim cannot proceed without: the
    // route rejects it, and a checkpoint attributed to `claude:` would be a row
    // that names no call. The drift report above has already fired.
    if tool_name.is_empty() {
        return;
    }

    // Deliberately NOT filtered here against `toolclass::mutates_fs`. The
    // matcher installed at spawn is the pre-filter and the app-side route is the
    // authority (it re-checks the same table); adding a third copy of the rule
    // in the shim would be a place for the three to drift, and this process is
    // the one that must stay cheapest and least clever.
    let body = serde_json::json!({
        "tab": tab,
        "agent": CONSUMER,
        "tool": tool_name,
        "cwd": resolve_cwd(cwd_raw),
        "session_id": session_id,
    })
    .to_string();

    // **AWAITED** — the 2026-08-13 amendment. Claude does not start the tool
    // until this process exits, so blocking here is what makes "the checkpoint
    // precedes the edit" true instead of likely. Nothing is written to stdout or
    // stderr on any path, so this still cannot deny the call; see the module doc
    // for the whole argument, including why the sibling beacon must NOT do this.
    dispatch("/workbench/tool_checkpoint", &body, Reply::Await);
}

/// Send one loopback POST, optionally waiting for the app's answer.
///
/// A near-twin of [`crate::taint_beacon`]'s dispatcher and kept separate rather
/// than shared, because the two shims' contracts are what justify the shape —
/// that one must never read a reply, this one must. Sharing would mean one
/// helper carrying both, i.e. exactly the switch a future "simplification" would
/// flip in the wrong direction.
///
/// Every failure — no running instance, a refused connect, a partial write, a
/// peer that closes without answering — is swallowed, and none of them consumes
/// the reply budget: they fail immediately. A lost checkpoint costs one tool
/// call its own rewind point (the previous checkpoint still covers the turn); a
/// blocked `Edit` breaks the tab.
///
/// Discovery is root-aware by this process's own cwd, exactly like
/// `context_hook::post_loopback`: Claude spawns hook shims in the project
/// directory, so with several cImp instances off one install the POST reaches
/// the instance serving ITS project — which for this route is load-bearing
/// beyond routing, since the wrong instance would checkpoint the wrong repo.
fn dispatch(path: &str, body: &str, reply: Reply) {
    let cwd = std::env::current_dir().ok();
    let Some(disc) = crate::offload::loopback::read_discovery_for(cwd.as_deref()) else {
        return;
    };
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), disc.port);
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, DISPATCH_TIMEOUT) else {
        // The memoized endpoint passed a probe and is now refusing connections.
        // Drop it so this process's second dispatch (a contract-drift report is
        // followed by the checkpoint itself) re-resolves instead of inheriting
        // it. Same reasoning as `taint_beacon::dispatch`.
        crate::offload::loopback::forget_resolved_discovery();
        return;
    };
    if stream.set_write_timeout(Some(DISPATCH_TIMEOUT)).is_err() {
        return;
    }
    // Set BEFORE the write, so a peer that answers between our last write byte
    // and our first read cannot be read with no deadline in force.
    if reply == Reply::Await && stream.set_read_timeout(Some(REPLY_TIMEOUT)).is_err() {
        return;
    }
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        disc.token,
        body.len()
    );
    let _ = stream.write_all(req.as_bytes());
    if reply == Reply::Await {
        // The bytes are discarded — the app's answer carries `checkpointed`,
        // and this process has nothing it may do with it: reporting a miss is
        // the app's job (it is the side that knows), and this shim writing
        // ANYTHING to stdout or stderr is the one thing that could turn a
        // report-only hook into a denial. What is load-bearing is the WAIT: the
        // app writes its reply only after `on_tool` has returned, so reaching
        // this point means the snapshot settled before Claude ran the tool.
        //
        // `read_to_end` rather than a header parse, for the same reason:
        // nothing is parsed, so there is nothing to get wrong, and the peer
        // sends `Connection: close` so EOF arrives on its own.
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    }
    // The socket closes on drop; `Connection: close` already told the peer not
    // to expect reuse.
}

/// A payload string field, or `""` when absent/not-a-string.
fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|f| f.as_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::offload::loopback::{DISCOVERY_PROBE_TIMEOUT, MAX_DISCOVERY_PROBES};
    use serde_json::json;

    /// The drift checks name the three fields this shim depends on, and a
    /// complete payload reports nothing (the happy path never POSTs a drift
    /// row).
    #[test]
    fn contract_checks_cover_the_fields_the_checkpoint_reads() {
        let full = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s-1",
            "cwd": "P:/proj",
            "tool_name": "Edit",
        });
        let checks = |v: &serde_json::Value| {
            missing_fields(&[
                ("tool_name", !str_field(v, "tool_name").is_empty()),
                ("session_id", !str_field(v, "session_id").is_empty()),
                ("cwd", !str_field(v, "cwd").is_empty()),
            ])
        };
        assert!(checks(&full).is_empty());
        assert_eq!(
            checks(&json!({})),
            vec!["tool_name", "session_id", "cwd"],
            "all three are reported, not just the first"
        );
        // A non-string field reads as absent rather than panicking.
        assert_eq!(str_field(&json!({ "tool_name": 7 }), "tool_name"), "");
    }

    /// **Every tool the spawn-time matcher names must be one the app-side route
    /// will accept.** The matcher and `toolclass::TABLE` are edited in different
    /// files, and a matcher naming a tool with no `mutates_fs: true` row spawns
    /// a process per call whose POST the route answers `checkpointed: false` —
    /// a silently dead seam, which is the failure mode this whole phase exists
    /// to avoid.
    ///
    /// The reverse direction is deliberately NOT asserted: `run_command` is
    /// mutating and is not a Claude tool at all, so the table is legitimately
    /// wider than the matcher.
    /// **The two-timer relationship the 2026-08-13 amendment rests on.**
    ///
    /// This shim's wait must be strictly LONGER than the app's own budget, or
    /// the guarantee inverts: Claude starts the tool the moment this process
    /// exits, so if the shim gave up first the app would still be staging into
    /// the running tool call while believing its checkpoint was taken before it
    /// — the exact race the amendment closes. The two constants live in
    /// different files, so nothing but this assertion keeps them ordered.
    ///
    /// The connect/write budget is asserted small for the other half of the
    /// argument: only a LIVE app may spend the reply budget, and an unreachable
    /// one must still fail fast.
    #[test]
    fn the_shim_waits_longer_than_the_app_takes_to_give_up() {
        assert!(
            REPLY_TIMEOUT > crate::offload::loopback::TOOL_CHECKPOINT_BUDGET,
            "the app must answer before the shim stops listening, or an abandoned \
             snapshot and a still-running one become indistinguishable to Claude"
        );
        assert!(DISPATCH_TIMEOUT < REPLY_TIMEOUT);
        // **The whole worst case must fit under the harness-side ceiling
        // `tabs::config` writes for this hook (`"timeout": 5`, seconds)** — and
        // "the whole worst case" includes the cold discovery resolution this
        // process pays BEFORE the POST, which is the term a reader (and the
        // pre-amendment version of this comment) forgets. 1.2 s of probing +
        // 80 ms connect + 80 ms write + 2 s reply = 3.36 s, so the margin is
        // real but no longer generous: raising any of the four needs this
        // assertion re-checked rather than a guess.
        let worst = DISCOVERY_PROBE_TIMEOUT * MAX_DISCOVERY_PROBES as u32
            + DISPATCH_TIMEOUT * 2
            + REPLY_TIMEOUT;
        assert!(
            worst < Duration::from_secs(5),
            "a hook that can outlast its own harness ceiling has an undefined \
             failure mode (see the taint beacon's timeout-semantics note): {worst:?}"
        );
    }

    #[test]
    fn every_matched_claude_tool_is_classified_as_mutating() {
        for tool in crate::tabs::config::CLAUDE_MUTATING_TOOL_MATCHER.split('|') {
            assert!(
                crate::offload::toolclass::mutates_fs(tool),
                "`{tool}` is in the PreToolUse matcher but has no `mutates_fs: true` row — \
                 the shim would spawn per call and the route would refuse every one"
            );
        }
    }
}
