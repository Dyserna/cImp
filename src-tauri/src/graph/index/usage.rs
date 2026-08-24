//! V14 Phase C/D — the **token/cost accounting ring** (`usage_stat`) and every
//! query the Usage surfaces read it with.
//!
//! One relation, written by exactly one method ([`GraphIndex::record_usage_event`])
//! and read by a dozen. It is a MEMORY relation: ensured outside `RELATIONS` so
//! a full index rebuild never wipes it, ring-bounded per session, and evicted
//! with its session by the cascade in [`super::memory`].
//!
//! # Why this is its own module (V42 R13)
//!
//! Two reasons beyond size. The first is that `usage_stat`'s **stored row shape
//! is frozen** — four `Int` token columns plus a `String` origin, sitting on
//! disk in every existing store — and the whole of the read boundary that maps
//! it onto whatever a harness declares lives here (`row_columns`,
//! `column_kinds`, [`COLUMN_KINDS`]). Keeping the shape, its migration and its
//! readers in one file is what makes "the persisted row shape does not move"
//! reviewable.
//!
//! The second is the string `"tool_result"`. It is cImp's own `kind`
//! discriminator on this relation, chosen long before V35, and it collides with
//! the Claude payload field of the same name — which is why
//! `harness::layering`'s `LITERAL_ALLOWLIST` carries a row for the file that
//! holds it. That exemption covers a WHOLE file, so the smaller the file, the
//! narrower the exemption: it used to cover 9,000 lines of `graph/index.rs` and
//! now covers this one, which reads no harness payload at all.

use std::collections::{BTreeMap, HashMap};

use cozo::{DataValue, Num, ScriptMutability};

use crate::error::AppResult;
use crate::graph::memory::{
    ColumnTotals, ModelUsage, SessionInfo, SessionUsageRow, TurnUsage, UsageEvent,
    MAX_USAGE_PER_SESSION,
};

use super::memory::prune_sessions_in_tx;
use super::{cell_i64, cell_str, dv_string, tx_put, tx_run, GraphIndex, StageAndSwap};

impl GraphIndex {
    /// The `usage_stat` relation's `:create` DDL, parameterized by relation
    /// `name` so the live relation ([`Self::ensure_memory_relations`]) and the
    /// V24 migration stage ([`Self::migrate_usage_stat_origin`]) share one
    /// source of truth — the V24 shape, carrying the `origin` column. The body
    /// stays byte-identical across both call sites; only the name differs.
    pub(super) fn usage_stat_create_ddl(name: &str) -> String {
        format!(
            ":create {name} {{session_id: String, seq: Int => \
                kind: String, model: String?, msg_id: String?, \
                in_tok: Int, out_tok: Int, cache_read: Int, cache_make: Int, \
                tool: String?, chars: Int, ts_ms: Int, origin: String}}"
        )
    }

    /// The migration stage relation used by [`Self::migrate_usage_stat_origin`]
    /// — a fully-populated new-shape copy built (atomically, `:create … <- $rows`)
    /// before the old `usage_stat` is ever dropped, so its presence on open
    /// always means "a prior migration was interrupted mid-swap; adopt me".
    const USAGE_STAT_STAGE: &'static str = "usage_stat_v24";

    /// V24 Phase A: add the `origin` column to a pre-V24 `usage_stat` relation,
    /// defaulting existing rows to `"session"` (forward-only S/A attribution).
    /// Recreate-and-copy because CozoDB has no `ALTER`. A no-op when the
    /// relation is absent (a brand-new store — `ensure_memory_relations` already
    /// made the new shape) or already carries `origin` (re-run / fresh store),
    /// detected by column introspection so calling it is always safe. Called
    /// from the writable [`Self::open`] migration path; NOT a `RELATIONS` reset
    /// (that never touches memory relations).
    ///
    /// The crash-safety is [`Self::stage_and_swap`]'s — see it for why the
    /// sequence is shaped this way. Everything below is what is specific to this
    /// relation: which columns the old shape had, and what the new one defaults
    /// to.
    pub(super) fn migrate_usage_stat_origin(&self) -> AppResult<()> {
        self.stage_and_swap(StageAndSwap {
            live: "usage_stat",
            stage: Self::USAGE_STAT_STAGE,
            added_column: "origin",
            read_script:
                "?[session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms] := \
                *usage_stat{session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms}",
            defaults: &[DataValue::Str("session".into())],
            stage_columns:
                "session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, \
                 tool, chars, ts_ms, origin",
            stage_ddl: &Self::usage_stat_create_ddl(Self::USAGE_STAT_STAGE),
            count_stage: &|| self.usage_stat_row_count(Self::USAGE_STAT_STAGE),
            promote: &|| self.promote_usage_stat_stage(),
        })
    }

    /// Promote a fully-populated migration stage ([`Self::USAGE_STAT_STAGE`]) to
    /// `usage_stat`: drop whatever `usage_stat` currently is (a stale old-shape
    /// relation, or the empty new-shape one `ensure_memory_relations` recreates
    /// after an interrupted swap) and rename the stage over it. Idempotent on
    /// retry — a crash between the drop and the rename leaves the durable stage,
    /// which the next open re-promotes.
    fn promote_usage_stat_stage(&self) -> AppResult<()> {
        if self.existing_relations()?.contains("usage_stat") {
            self.run_mut("::remove usage_stat", BTreeMap::new())?;
        }
        self.run_mut(
            &format!("::rename {} -> usage_stat", Self::USAGE_STAT_STAGE),
            BTreeMap::new(),
        )?;
        Ok(())
    }

    /// The number of stored `usage_stat`-shaped rows in `name`, counted by its
    /// `(session_id, seq)` primary key so CozoScript's set-projection semantics
    /// can't dedupe distinct rows into an undercount (the `seq`-keeps-rows-
    /// distinct reasoning on [`Self::usage_session_totals`]). Only used for
    /// migration verification.
    fn usage_stat_row_count(&self, name: &str) -> AppResult<usize> {
        let rows = self.run(
            &format!("?[session_id, seq] := *{name}{{session_id, seq}}"),
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.len())
    }

    /// Whether the on-disk `usage_stat` relation carries the V24 `origin`
    /// column.
    ///
    /// Test-only since V42 R12: the migration itself asks
    /// [`Self::relation_has_column`] through [`StageAndSwap::added_column`], so
    /// what is left here is the assertion the two migration tests make about a
    /// migrated store. Kept (rather than inlined into them) so those tests read
    /// exactly as they did before the engine was folded.
    #[cfg(test)]
    fn usage_stat_has_origin(&self) -> AppResult<bool> {
        self.relation_has_column("usage_stat", "origin")
    }

    // ── V14 Phase C: usage / cost accounting ──────────────────────────────

    /// Append one usage event for `session_id`: upserts the session's
    /// last-seen time (same shape as `record_mem_event` — usage rows are
    /// often the FIRST activity a chat-only turn ever produces, so this tap
    /// must be able to create the `session` row on its own, not just piggy-
    /// back on a tool call). A `Turn` event is UPSERTED in place when a row
    /// for its `msg_id` already exists (a streamed message's `usage` block
    /// firming up across updates); every other write appends a new row.
    /// Ring-prunes beyond [`MAX_USAGE_PER_SESSION`], then evicts old sessions
    /// via the same cascade `record_mem_event` uses — all in one write
    /// transaction so the monotonic `seq` allocation is race-free.
    pub fn record_usage_event(
        &self,
        session_id: &str,
        agent: &str,
        event: &UsageEvent,
        ts_ms: i64,
    ) -> AppResult<()> {
        let sid = session_id.to_string();
        let agent = agent.to_string();
        let event = event.clone();
        self.with_write_txn(move |tx| {
            let mut p = BTreeMap::new();
            p.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));

            // Upsert the session (identical shape to `record_mem_event`).
            let existing = tx_run(
                tx,
                "?[started_ms] := *session{session_id: $sid, started_ms}",
                p.clone(),
            )?;
            let started = existing.rows.first().map(|r| cell_i64(r, 0)).unwrap_or(ts_ms);
            let mut ps = BTreeMap::new();
            ps.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
            ps.insert("agent".to_string(), DataValue::Str(agent.as_str().into()));
            ps.insert("st".to_string(), DataValue::Num(Num::Int(started)));
            ps.insert("last".to_string(), DataValue::Num(Num::Int(ts_ms)));
            tx_run(
                tx,
                "?[session_id, agent, started_ms, last_ms] <- [[$sid, $agent, $st, $last]]\n\
                 :put session {session_id => agent, started_ms, last_ms}",
                ps,
            )?;

            // A "turn" event upserts by msg_id: look for an existing row's
            // seq first so we overwrite in place rather than append.
            let existing_seq = if let UsageEvent::Turn { msg_id, .. } = &event {
                let mut pm = p.clone();
                pm.insert("mid".to_string(), DataValue::Str(msg_id.as_str().into()));
                let rows = tx_run(
                    tx,
                    "?[seq] := *usage_stat{session_id: $sid, seq, kind, msg_id}, \
                        kind == \"turn\", msg_id == $mid\n:limit 1",
                    pm,
                )?;
                rows.rows.first().map(|r| cell_i64(r, 0))
            } else {
                None
            };

            let seq = match existing_seq {
                Some(s) => s,
                None => {
                    let rows = tx_run(
                        tx,
                        "?[count(seq), max(seq)] := *usage_stat{session_id: $sid, seq}",
                        p.clone(),
                    )?;
                    let (cnt, mx) =
                        rows.rows.first().map(|r| (cell_i64(r, 0), cell_i64(r, 1))).unwrap_or((0, 0));
                    if cnt == 0 { 0 } else { mx + 1 }
                }
            };

            // `origin` is meaningful only for a "turn" row; a "tool_result" row
            // is sized in chars and not per-turn attributed, so it stores the
            // neutral `"session"` (the column is non-nullable).
            // The ascription IS the `usage_stat` column list — it is what makes
            // the `None`/`0` arms infer; a named alias would only re-spell it.
            #[allow(clippy::type_complexity)]
            let (kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, origin): (
                &str,
                Option<String>,
                Option<String>,
                i64,
                i64,
                i64,
                i64,
                Option<String>,
                i64,
                &str,
            ) = match &event {
                UsageEvent::Turn { msg_id, model, in_tok, out_tok, cache_read, cache_make, origin } => (
                    "turn",
                    model.clone(),
                    Some(msg_id.clone()),
                    *in_tok as i64,
                    *out_tok as i64,
                    *cache_read as i64,
                    *cache_make as i64,
                    None,
                    0,
                    origin.as_str(),
                ),
                UsageEvent::ToolResult { tool, chars } => {
                    ("tool_result", None, None, 0, 0, 0, 0, tool.clone(), *chars as i64, "session")
                }
            };

            let row = DataValue::List(vec![
                DataValue::Str(sid.as_str().into()),
                DataValue::Num(Num::Int(seq)),
                DataValue::Str(kind.into()),
                model.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                msg_id.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                DataValue::Num(Num::Int(in_tok)),
                DataValue::Num(Num::Int(out_tok)),
                DataValue::Num(Num::Int(cache_read)),
                DataValue::Num(Num::Int(cache_make)),
                tool.as_deref().map(|s| DataValue::Str(s.into())).unwrap_or(DataValue::Null),
                DataValue::Num(Num::Int(chars)),
                DataValue::Num(Num::Int(ts_ms)),
                DataValue::Str(origin.into()),
            ]);
            tx_put(
                tx,
                "?[session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin] <- $rows\n\
                 :put usage_stat {session_id, seq => kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin}",
                vec![row],
            )?;

            // Ring-prune this session's oldest usage rows beyond the cap
            // (same inline-bind + head unification as `record_mem_event`'s).
            let cutoff = seq - MAX_USAGE_PER_SESSION;
            if cutoff >= 0 {
                let mut pc = BTreeMap::new();
                pc.insert("sid".to_string(), DataValue::Str(sid.as_str().into()));
                pc.insert("cut".to_string(), DataValue::Num(Num::Int(cutoff)));
                tx_run(
                    tx,
                    "?[session_id, seq] := *usage_stat{session_id: $sid, seq}, seq <= $cut, session_id = $sid\n:rm usage_stat {session_id, seq}",
                    pc,
                )?;
            }

            // Evict sessions beyond the per-root cap (cascade events + usage +
            // unpinned notes; pinned notes survive).
            prune_sessions_in_tx(tx)?;
            Ok(())
        })
    }

    /// **The session's harness's declared turn shape**, or `None` when the
    /// session's `agent` names no registered harness (or names one that
    /// declares no shape).
    ///
    /// Resolved ONCE per usage query and threaded through the row loop — the
    /// registry lookup is a linear scan over the descriptors and a per-row call
    /// would repeat it thousands of times on a long session.
    fn session_turn_shape(
        &self,
        session_id: &str,
    ) -> AppResult<Option<&'static crate::harness::plugin::TurnUsageShape>> {
        Ok(self
            .session_agent(session_id)?
            .and_then(|a| crate::harness::HarnessId::from_id(&a))
            .and_then(|h| h.plugin())
            .and_then(|p| p.turn_usage_shape()))
    }

    /// Whether `session_id` has any recorded `turn` usage row — the V24 Phase E
    /// signal that exact token accounting exists (see the `est_only` derivation
    /// in [`Self::usage_row_for_session`]). A `:limit 1` existence probe, so it
    /// stays cheap even on a long session's ring, and detects a recorded turn
    /// even when its tokens are all zero (unlike summing `usage_session_totals`).
    fn usage_session_has_turn(&self, session_id: &str) -> AppResult<bool> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[seq] := *usage_stat{session_id: $sid, seq, kind}, kind == \"turn\"\n:limit 1",
            p,
            ScriptMutability::Immutable,
        )?;
        Ok(!rows.rows.is_empty())
    }

    /// Summed token totals for `session_id` across its "turn" rows, as the
    /// **declared-shape** payload type ("tool_result" rows carry chars, not
    /// tokens, so they don't contribute).
    ///
    /// The harness is resolved once here; see [`column_kinds`] for which
    /// categories the answer carries.
    pub fn usage_session_totals(
        &self,
        session_id: &str,
    ) -> AppResult<crate::harness::plugin::TokenKinds> {
        let shape = self.session_turn_shape(session_id)?;
        Ok(column_kinds(self.usage_column_totals(session_id)?, shape))
    }

    /// The four stored token columns for `session_id`, summed — the internal
    /// aggregate behind [`Self::usage_session_totals`] and the `cache_hit_ratio`
    /// / `est_only` derivations in [`Self::usage_row_for_session`], which need
    /// the raw columns whatever the harness declared.
    fn usage_column_totals(&self, session_id: &str) -> AppResult<ColumnTotals> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // `seq` MUST stay in the projection even though it's unused below:
        // CozoScript relations are SETS, so two turns with identical token
        // counts would otherwise collapse into one row and undercount (the
        // same reason `mem_working_set` keeps `seq` in its own projection).
        // `session_id` is bound INLINE (it is `usage_stat`'s leading key) —
        // a `session_id == $sid` post-filter would scan every session's rows;
        // this is the per-session pattern the whole file uses.
        let rows = self.run(
            "?[seq, in_tok, out_tok, cache_read, cache_make] := \
                *usage_stat{session_id: $sid, seq, kind, in_tok, out_tok, cache_read, cache_make}, \
                kind == \"turn\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut t = ColumnTotals::default();
        for r in &rows.rows {
            t.add(row_columns(r, 1));
        }
        Ok(t)
    }

    /// Distinct model ids across `session_id`'s turns, descending by the
    /// total tokens (input + output + cache) attributed to each. Turns with
    /// no model recorded are skipped, as are the harnesses' declared
    /// pseudo-models — a harness stamps one on locally fabricated messages
    /// (errors, interrupts) and it would pollute a "which model ran this
    /// session" readout. V40 Phase D: the list comes from
    /// [`crate::harness::is_model_sentinel`], not from a literal here.
    pub fn usage_session_models(&self, session_id: &str) -> AppResult<Vec<String>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // Same `seq`-keeps-rows-distinct and inline-`session_id` reasoning as
        // `usage_session_totals`.
        let rows = self.run(
            "?[seq, model, in_tok, out_tok, cache_read, cache_make] := \
                *usage_stat{session_id: $sid, seq, kind, model, in_tok, out_tok, cache_read, cache_make}, \
                kind == \"turn\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut sums: HashMap<String, u64> = HashMap::new();
        for r in &rows.rows {
            let Some(model) = cell_str_opt(r, 1) else {
                continue;
            };
            if crate::harness::is_model_sentinel(&model) {
                continue;
            }
            let toks: u64 = (2..=5).map(|i| cell_i64(r, i).max(0) as u64).sum();
            *sums.entry(model).or_insert(0) += toks;
        }
        let mut out: Vec<(String, u64)> = sums.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out.into_iter().map(|(m, _)| m).collect())
    }

    /// V24 Phase B: per-model token totals for `session_id` with the per-lane
    /// split, ordered by total tokens (every column summed) descending, model
    /// id breaking ties. Like
    /// [`Self::usage_session_models`] but keeps the sums it discards — the Cost
    /// card prices each model in a mixed-model session separately, and the
    /// `SessionUsageRow` cost badge sums per-model auto-matched rates. Same
    /// sentinel exclusion and no-model skip as `usage_session_models`,
    /// and the same `seq`-keeps-rows-distinct reasoning as
    /// [`Self::usage_session_totals`].
    pub fn usage_session_model_totals(&self, session_id: &str) -> AppResult<Vec<ModelUsage>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[seq, model, in_tok, out_tok, cache_read, cache_make, origin] := \
                *usage_stat{session_id: $sid, seq, kind, model, in_tok, out_tok, cache_read, cache_make, origin}, \
                kind == \"turn\"",
            p,
            ScriptMutability::Immutable,
        )?;
        // Resolved ONCE for the whole query, not per row.
        let shape = self.session_turn_shape(session_id)?;
        struct Agg {
            totals: ColumnTotals,
            /// Total tokens per STORED lane id, read back verbatim. A lane no
            /// row carried gets no entry — the closed `OriginSplit` this
            /// replaced always emitted both halves, so a single-lane harness
            /// rendered a fabricated second lane at 0.
            origins: BTreeMap<String, u64>,
        }
        let mut map: HashMap<String, Agg> = HashMap::new();
        for r in &rows.rows {
            let Some(model) = cell_str_opt(r, 1) else {
                continue;
            };
            if crate::harness::is_model_sentinel(&model) {
                continue;
            }
            let cols = row_columns(r, 2);
            let e = map.entry(model).or_insert_with(|| Agg {
                totals: ColumnTotals::default(),
                origins: BTreeMap::new(),
            });
            e.totals.add(cols);
            *e.origins.entry(cell_str(r, 6)).or_insert(0) += cols.total();
        }
        let mut out: Vec<(u64, ModelUsage)> = map
            .into_iter()
            .map(|(model, a)| {
                (
                    a.totals.total(),
                    ModelUsage {
                        model,
                        totals: column_kinds(a.totals, shape),
                        origins: a.origins,
                    },
                )
            })
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.model.cmp(&b.1.model)));
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }

    /// Estimated tool-result characters for `session_id`, grouped by tool
    /// name (`"unknown"` when the id → name join missed — see the claude
    /// tap's `ToolNameRing`), descending by chars.
    pub fn usage_per_tool(&self, session_id: &str) -> AppResult<Vec<(String, u64)>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // Same `seq`-keeps-rows-distinct reasoning as `usage_session_totals`:
        // without it, two tool results with identical (tool, chars) — e.g.
        // two 1-char Bash results — would collapse into one row. Same inline
        // `session_id` prefix bind, too.
        let rows = self.run(
            "?[seq, tool, chars] := *usage_stat{session_id: $sid, seq, kind, tool, chars}, \
                kind == \"tool_result\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut sums: HashMap<String, u64> = HashMap::new();
        for r in &rows.rows {
            let tool = cell_str_opt(r, 1).unwrap_or_else(|| "unknown".to_string());
            *sums.entry(tool).or_insert(0) += cell_i64(r, 2).max(0) as u64;
        }
        let mut out: Vec<(String, u64)> = sums.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Per-turn token breakdown for `session_id`, ordered oldest → newest.
    /// Each turn's `tool_chars` is the (estimated) tool-result characters
    /// that arrived AFTER the previous turn and before this one — i.e. the
    /// tool output this turn's assistant message actually read as input
    /// context. Tool-result rows after the LAST turn (mid-turn, the next
    /// assistant reply hasn't landed yet) aren't attributable to a turn yet
    /// and are dropped from this series; they still count toward
    /// `usage_per_tool`'s totals.
    pub fn usage_turn_series(&self, session_id: &str) -> AppResult<Vec<TurnUsage>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin] := \
                *usage_stat{session_id: $sid, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin}\n\
                :order seq",
            p,
            ScriptMutability::Immutable,
        )?;
        // Resolved ONCE for the whole series, not per turn.
        let shape = self.session_turn_shape(session_id)?;
        let mut out = Vec::new();
        let mut pending_tool_chars: u64 = 0;
        for r in &rows.rows {
            if cell_str(r, 1) == "tool_result" {
                pending_tool_chars += cell_i64(r, 9).max(0) as u64;
                continue;
            }
            out.push(TurnUsage {
                msg_id: cell_str_opt(r, 3).unwrap_or_default(),
                model: cell_str_opt(r, 2),
                tokens: column_kinds(row_columns(r, 4), shape),
                tool_chars: pending_tool_chars,
                ts_ms: cell_i64(r, 10),
                // Read back VERBATIM: whatever the producing harness declared
                // is what the lane is. There is no `from_wire` defaulting any
                // more — mapping an unrecognised id onto "the main session"
                // silently merged a third harness's lane into somebody else's
                // spend.
                origin: cell_str(r, 11),
            });
            pending_tool_chars = 0;
        }
        Ok(out)
    }

    /// One row per known session with usage token totals, cache-hit ratio,
    /// and whether the session is estimate-only (no exact `usage` block —
    /// currently every non-Claude agent; see the OpenCode C3 spike note atop
    /// `harness/opencode/read.rs`). Reuses [`Self::mem_sessions`] for the session list
    /// so a session with usage but zero classified `mem_event`s still shows.
    pub fn usage_all_sessions(&self) -> AppResult<Vec<SessionUsageRow>> {
        let sessions = self.mem_sessions()?;
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            out.push(self.usage_row_for_session(s)?);
        }
        Ok(out)
    }

    /// The single [`SessionUsageRow`] for `session_id`, or `None` when no
    /// `session` row exists for that id (unknown session — V24 Phase B
    /// drill-in). Same shape as one entry of [`Self::usage_all_sessions`],
    /// built for one id so the `graph_session_usage` command doesn't scan every
    /// session to render one.
    pub fn usage_session_row(&self, session_id: &str) -> AppResult<Option<SessionUsageRow>> {
        let Some(info) = self
            .mem_sessions()?
            .into_iter()
            .find(|s| s.session_id == session_id)
        else {
            return Ok(None);
        };
        Ok(Some(self.usage_row_for_session(info)?))
    }

    /// Build one session's totals row from its [`SessionInfo`] — the shared
    /// body of [`Self::usage_all_sessions`] and [`Self::usage_session_row`].
    fn usage_row_for_session(&self, s: SessionInfo) -> AppResult<SessionUsageRow> {
        let cols = self.usage_column_totals(&s.session_id)?;
        // The session row already carries its `agent`, so the shape resolves
        // without a second `session_agent` query.
        let shape = crate::harness::HarnessId::from_id(&s.agent)
            .and_then(|h| h.plugin())
            .and_then(|p| p.turn_usage_shape());
        let totals = column_kinds(cols, shape);
        let per_tool = self.usage_per_tool(&s.session_id)?;
        let models = self.usage_session_models(&s.session_id)?;
        let tool_chars: u64 = per_tool.iter().map(|(_, c)| *c).sum();
        // Derived from the raw COLUMNS, not from the declared-shape payload: a
        // harness that does not declare `cache_read` has no ratio to show, and
        // reading an absent category as 0 here would print a confident "0%".
        let denom = cols.cache_read + cols.in_tok;
        let cache_hit_ratio = if denom > 0 {
            cols.cache_read as f64 / denom as f64
        } else {
            0.0
        };
        // V24 Phase E: "est" means "no real token accounting", derived from
        // whether ANY turn was recorded — not from the summed token totals. A
        // recorded turn (Claude, or OpenCode once its plugin forwards usage in
        // Phase F) means exact accounting exists even when that turn's tokens are
        // all zero: a Claude API-error line lands a tolerant zero-token turn (see
        // `parse_usage_line`), and summing-to-zero would have mis-flagged it as
        // est. Only a session with no turn rows at all (pre-V24 OpenCode,
        // tool-result chars only) keeps the badge. Both `usage_all_sessions` and
        // `usage_session_row` go through here, so the two paths derive it
        // identically.
        let est_only = !self.usage_session_has_turn(&s.session_id)?;
        Ok(SessionUsageRow {
            est_only,
            session_id: s.session_id,
            agent: s.agent,
            totals,
            tool_chars,
            cache_hit_ratio,
            started_ms: s.started_ms,
            last_ms: s.last_ms,
            models,
        })
    }

    /// V14 Phase D: per-tool ranking for the Usage section's "top consumers"
    /// table: `(tool, chars, calls)` descending by chars. Distinct from
    /// [`Self::usage_per_tool`] (chars-only, feeds `SessionUsageRow.tool_chars`)
    /// because the table also wants a call count.
    pub fn usage_tool_ranking(&self, session_id: &str) -> AppResult<Vec<(String, u64, u64)>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        // `seq` stays in the projection for the same set-semantics reason as
        // `usage_per_tool` — two identical (tool, chars) tool-result rows
        // must not collapse into one — and `session_id` binds inline for the
        // same prefix-scan reason.
        let rows = self.run(
            "?[seq, tool, chars] := *usage_stat{session_id: $sid, seq, kind, tool, chars}, \
                kind == \"tool_result\"",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut sums: HashMap<String, (u64, u64)> = HashMap::new();
        for r in &rows.rows {
            let tool = cell_str_opt(r, 1).unwrap_or_else(|| "unknown".to_string());
            let entry = sums.entry(tool).or_insert((0, 0));
            entry.0 += cell_i64(r, 2).max(0) as u64;
            entry.1 += 1;
        }
        let mut out: Vec<(String, u64, u64)> = sums
            .into_iter()
            .map(|(tool, (chars, calls))| (tool, chars, calls))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }
}

// ── usage_stat's four token columns → declared token categories ────────────
//
// **The persisted row shape does not move.** `usage_stat` is a memory relation
// that survives an index rebuild and sits on disk in every existing user's
// `graph.db` with exactly four `Int` token columns plus a `String` origin. V40
// Phase G generalises the PAYLOAD, not the storage: the conversion below is the
// whole of the read boundary, and there is deliberately no graph migration.
//
// The four ids these columns map onto — `input` / `output` / `cache_read` /
// `cache_write` — are **cImp's own provider/pricing vocabulary, not a
// harness's**. Locked decision 29 rules the price table provider knowledge, and
// `crate::pricing`'s rate fields are spelled with these exact four names; a
// stored column means "tokens billed in this category", so naming them in core
// is naming core's own table. What a HARNESS declares is which of them it
// reports — see [`crate::harness::plugin::TurnUsageShape`].

/// The pricing category each stored column bills under, in column order:
/// `in_tok`, `out_tok`, `cache_read`, `cache_make`.
const COLUMN_KINDS: [&str; 4] = ["input", "output", "cache_read", "cache_write"];

/// The four token columns of one `usage_stat` row, starting at projection
/// index `base` (`in_tok`, `out_tok`, `cache_read`, `cache_make` — the order
/// every usage query projects them in). Negative stored values clamp to 0.
fn row_columns(row: &[DataValue], base: usize) -> ColumnTotals {
    ColumnTotals {
        in_tok: cell_i64(row, base).max(0) as u64,
        out_tok: cell_i64(row, base + 1).max(0) as u64,
        cache_read: cell_i64(row, base + 2).max(0) as u64,
        cache_make: cell_i64(row, base + 3).max(0) as u64,
    }
}

/// **Stored columns → the declared-shape payload**, applying the absence rule.
///
/// A category is emitted when the session's harness DECLARES it (`shape`), even
/// at zero — a harness that bills cache reads and read none this session really
/// did read zero, and the donut's four segments are its own statement. When the
/// harness is unknown or declares no shape, only the columns that are actually
/// non-zero are emitted: core has nobody's word for what the other categories
/// mean, and inventing four keys would be the "empty is not absent" defect this
/// type exists to prevent (global principle 5).
///
/// For the two shipped harnesses (both declare all four) this reproduces
/// exactly the numbers the old four-field `UsageTotals` carried.
fn column_kinds(
    cols: ColumnTotals,
    shape: Option<&'static crate::harness::plugin::TurnUsageShape>,
) -> crate::harness::plugin::TokenKinds {
    let mut out = crate::harness::plugin::TokenKinds::default();
    for (id, v) in COLUMN_KINDS.into_iter().zip([
        cols.in_tok,
        cols.out_tok,
        cols.cache_read,
        cols.cache_make,
    ]) {
        let emit = match shape {
            Some(s) => s.declares_kind(id),
            None => v != 0,
        };
        if emit {
            out.set(id, v);
        }
    }
    out
}

/// A nullable `String?` column: `None` for a stored `Null`/missing cell
/// (unlike [`cell_str`], which folds that case into `""`) — needed wherever
/// absence must stay distinguishable from an empty string (`usage_stat`'s
/// `model`/`msg_id`/`tool` columns).
fn cell_str_opt(row: &[DataValue], i: usize) -> Option<String> {
    match row.get(i) {
        None | Some(DataValue::Null) => None,
        Some(v) => Some(dv_string(v)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── V14 Phase C: usage_stat store ──────────────────────────────────────

    #[test]
    fn usage_turn_upserts_by_msg_id_instead_of_duplicating() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-upsert-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // A partial line (usage not yet firmed up) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: None,
                in_tok: 0,
                out_tok: 0,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            100,
        )
        .unwrap();
        // ... then the SAME message id with the real numbers AND a firmed-up
        // origin (a sub-agent line whose sidechain flag the first partial lacked
        // — the upsert must carry the new origin, not keep the stale one).
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: Some("claude-x".to_string()),
                in_tok: 120,
                out_tok: 30,
                cache_read: 40,
                cache_make: 5,
                origin: "agent".to_string(),
            },
            110,
        )
        .unwrap();

        let series = idx.usage_turn_series("s1").unwrap();
        assert_eq!(
            series.len(),
            1,
            "same msg_id must upsert in place, not duplicate"
        );
        assert_eq!(series[0].msg_id, "m1");
        assert_eq!(series[0].model.as_deref(), Some("claude-x"));
        // The stored columns come back as the harness's DECLARED categories
        // (V40 Phase G): `claude` declares all four, so all four are present
        // with exactly the numbers the four `UsageTotals` fields used to hold.
        assert_eq!(series[0].tokens.get("input"), Some(120));
        assert_eq!(series[0].tokens.get("output"), Some(30));
        assert_eq!(series[0].tokens.get("cache_read"), Some(40));
        assert_eq!(series[0].tokens.get("cache_write"), Some(5));
        assert_eq!(
            series[0].origin, "agent",
            "upsert carries the updated origin"
        );

        let totals = idx.usage_session_totals("s1").unwrap();
        assert_eq!(
            totals.get("input"),
            Some(120),
            "totals reflect the upserted (last) value, not both writes summed"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V40 Phase G: the declared-shape read boundary ─────────────────────

    /// **The persisted row shape did not move.**
    ///
    /// `usage_stat` is a memory relation: it survives an index rebuild and sits
    /// on disk in every existing user's `graph.db`. V40 Phase G generalises the
    /// PAYLOAD at the read boundary and adds no graph migration, so this pins
    /// the four `Int` token columns plus the `String` origin by name. A change
    /// here is a data migration, not a refactor.
    #[test]
    fn the_usage_stat_row_shape_is_unchanged() {
        let ddl = GraphIndex::usage_stat_create_ddl("usage_stat");
        assert_eq!(
            ddl,
            concat!(
                ":create usage_stat {session_id: String, seq: Int => ",
                "kind: String, model: String?, msg_id: String?, ",
                "in_tok: Int, out_tok: Int, cache_read: Int, cache_make: Int, ",
                "tool: String?, chars: Int, ts_ms: Int, origin: String}"
            ),
            "the on-disk usage row changed shape - that is a graph migration, and V40 Phase G deliberately performs none"
        );
        // And the live relation really is built from that DDL.
        let dir = std::env::temp_dir().join(format!("ckg-usage-ddl-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for col in ["in_tok", "out_tok", "cache_read", "cache_make", "origin"] {
            assert!(
                idx.relation_has_column("usage_stat", col).unwrap(),
                "the live relation lost the {col} column"
            );
        }
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A category the harness does not declare is ABSENT, not zero** — and an
    /// unknown harness emits only what it actually stored.
    ///
    /// The absence rule at the read boundary (locked decision 19; global
    /// principle 5). The pre-V40-G payload had four `u64` fields, so every
    /// session reported four categories whatever its harness billed, and a
    /// consumer could not tell "reported zero" from "never reported".
    #[test]
    fn an_undeclared_token_category_is_absent_from_a_stored_row() {
        use crate::harness::plugin::{TokenKindSpec, TurnOrigin, TurnUsageShape};
        let cols = ColumnTotals {
            in_tok: 100,
            out_tok: 20,
            cache_read: 0,
            cache_make: 7,
        };

        // A harness that bills input and output only: the two cache columns are
        // ABSENT even though one of them holds a real 7.
        static FLAT: TurnUsageShape = TurnUsageShape {
            token_kinds: &[
                TokenKindSpec { id: "input", label: "In" },
                TokenKindSpec { id: "output", label: "Out" },
            ],
            origins: &[TurnOrigin { id: "turn", label: "turns", subagent: false }],
        };
        let k = column_kinds(cols, Some(&FLAT));
        assert_eq!(k.get("input"), Some(100));
        assert_eq!(k.get("output"), Some(20));
        assert_eq!(k.get("cache_read"), None, "undeclared category must be absent");
        assert_eq!(k.get("cache_write"), None, "undeclared category must be absent");

        // A harness that declares all four gets all four — INCLUDING the zero,
        // because a category it does bill and spent nothing in really is zero.
        static FULL: TurnUsageShape = TurnUsageShape {
            token_kinds: &[
                TokenKindSpec { id: "input", label: "In" },
                TokenKindSpec { id: "cache_write", label: "CW" },
                TokenKindSpec { id: "cache_read", label: "CR" },
                TokenKindSpec { id: "output", label: "Out" },
            ],
            origins: &[TurnOrigin { id: "turn", label: "turns", subagent: false }],
        };
        let k = column_kinds(cols, Some(&FULL));
        assert_eq!(k.ids().collect::<Vec<_>>().len(), 4);
        assert_eq!(k.get("cache_read"), Some(0), "a DECLARED category at zero is a real zero");

        // No shape at all (unknown harness / one that records no turns): only
        // the columns that are non-zero, because core has nobody's word for
        // what the others mean.
        let k = column_kinds(cols, None);
        let mut ids: Vec<&str> = k.ids().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["cache_write", "input", "output"]);
        assert_eq!(k.get("cache_read"), None);

        // Nothing stored and nothing declared ⇒ nothing at all, not four zeros.
        assert!(column_kinds(ColumnTotals::default(), None).is_empty());
    }

    /// **A lane id no harness declares round-trips as itself.**
    ///
    /// The pre-V40-G `UsageOrigin::from_wire` mapped every unrecognised string
    /// onto `Session`, so a third harness's lane silently merged into the main
    /// session's spend. The column is read back verbatim now — including into
    /// `ModelUsage.origins`, whose closed `{ session_tok, agent_tok }` pair had
    /// no slot for it at all.
    #[test]
    fn an_undeclared_lane_round_trips_verbatim() {
        let dir = std::env::temp_dir().join(format!("ckg-lane-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for (msg, origin, in_tok) in [("m1", "session", 10u32), ("m2", "review", 5)] {
            idx.record_usage_event(
                "s1",
                "claude",
                &UsageEvent::Turn {
                    msg_id: msg.to_string(),
                    model: Some("model-a".to_string()),
                    in_tok,
                    out_tok: 0,
                    cache_read: 0,
                    cache_make: 0,
                    origin: origin.to_string(),
                },
                100,
            )
            .unwrap();
        }
        let series = idx.usage_turn_series("s1").unwrap();
        assert_eq!(
            series.iter().map(|t| t.origin.as_str()).collect::<Vec<_>>(),
            vec!["session", "review"],
            "an unrecognised lane must not be folded into the main session"
        );
        let per_model = idx.usage_session_model_totals("s1").unwrap();
        assert_eq!(per_model.len(), 1);
        assert_eq!(per_model[0].origins.get("session").copied(), Some(10));
        assert_eq!(per_model[0].origins.get("review").copied(), Some(5));
        // The lane the harness declares but this model never used has NO entry.
        assert_eq!(per_model[0].origins.get("agent"), None);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_stat_origin_migrates_from_a_pre_v24_store() {
        // V24 Phase A: a store whose `usage_stat` predates the `origin` column
        // must open cleanly, and its old turn rows must read `Session`.
        let dir = std::env::temp_dir().join(format!("ckg-usage-migr-{}", uuid::Uuid::new_v4()));
        {
            let idx = GraphIndex::open(&dir, ".ckg").expect("open");
            // Recreate `usage_stat` in the OLD (no-`origin`) shape and seed one
            // pre-V24 turn row directly, then stamp an older schema version so
            // the next `open()` takes the migration path.
            idx.run_mut("::remove usage_stat", BTreeMap::new()).unwrap();
            idx.run_mut(
                ":create usage_stat {session_id: String, seq: Int => \
                    kind: String, model: String?, msg_id: String?, \
                    in_tok: Int, out_tok: Int, cache_read: Int, cache_make: Int, \
                    tool: String?, chars: Int, ts_ms: Int}",
                BTreeMap::new(),
            )
            .unwrap();
            let old_row = DataValue::List(vec![
                DataValue::Str("s1".into()),
                DataValue::Num(Num::Int(0)),
                DataValue::Str("turn".into()),
                DataValue::Null,
                DataValue::Str("m1".into()),
                DataValue::Num(Num::Int(100)),
                DataValue::Num(Num::Int(10)),
                DataValue::Num(Num::Int(0)),
                DataValue::Num(Num::Int(0)),
                DataValue::Null,
                DataValue::Num(Num::Int(0)),
                DataValue::Num(Num::Int(100)),
            ]);
            let mut p = BTreeMap::new();
            p.insert("rows".to_string(), DataValue::List(vec![old_row]));
            idx.run_mut(
                "?[session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms] <- $rows\n\
                 :put usage_stat {session_id, seq => kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms}",
                p,
            )
            .unwrap();
            idx.write_schema_version(1).unwrap();
        }

        // The writable path migrates the column in place, no data loss.
        let idx2 = GraphIndex::open(&dir, ".ckg").expect("reopen");
        let series = idx2.usage_turn_series("s1").unwrap();
        assert_eq!(series.len(), 1, "the pre-V24 turn row survives migration");
        assert_eq!(series[0].msg_id, "m1");
        assert_eq!(
            series[0].tokens.get("input"),
            Some(100),
            "token counts are preserved"
        );
        assert_eq!(
            series[0].origin,
            "session",
            "old rows default to session"
        );
        // The relation now carries `origin`, so a re-open is a clean no-op.
        assert!(idx2.usage_stat_has_origin().unwrap());

        drop(idx2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_stat_migration_recovers_from_an_interrupted_swap() {
        // V24 code-review: simulate a crash mid-swap — the migration stage was
        // fully populated, the original `usage_stat` was dropped and recreated
        // EMPTY in the new shape by `ensure_memory_relations`, and the process
        // died before the stage was promoted. The next open must adopt the stage
        // (its rows), not the empty `usage_stat`, so no usage history is lost.
        let dir = std::env::temp_dir().join(format!("ckg-usage-recover-{}", uuid::Uuid::new_v4()));
        {
            let idx = GraphIndex::open(&dir, ".ckg").expect("open");
            // Build the fully-populated stage (new shape, origin already set) —
            // exactly what the atomic create-and-populate would have durably left.
            idx.run_mut(
                &GraphIndex::usage_stat_create_ddl(GraphIndex::USAGE_STAT_STAGE),
                BTreeMap::new(),
            )
            .unwrap();
            let staged_row = DataValue::List(vec![
                DataValue::Str("s1".into()),
                DataValue::Num(Num::Int(0)),
                DataValue::Str("turn".into()),
                DataValue::Null,
                DataValue::Str("m1".into()),
                DataValue::Num(Num::Int(100)),
                DataValue::Num(Num::Int(10)),
                DataValue::Num(Num::Int(0)),
                DataValue::Num(Num::Int(0)),
                DataValue::Null,
                DataValue::Num(Num::Int(0)),
                DataValue::Num(Num::Int(100)),
                DataValue::Str("session".into()),
            ]);
            let mut p = BTreeMap::new();
            p.insert("rows".to_string(), DataValue::List(vec![staged_row]));
            idx.run_mut(
                "?[session_id, seq, kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin] <- $rows\n\
                 :put usage_stat_v24 {session_id, seq => kind, model, msg_id, in_tok, out_tok, cache_read, cache_make, tool, chars, ts_ms, origin}",
                p,
            )
            .unwrap();
            // Leave `usage_stat` EMPTY in the new shape (what the interrupted swap
            // + the next `ensure_memory_relations` would have produced) and stamp
            // an older schema version so the reopen takes the migration path.
            idx.run_mut("::remove usage_stat", BTreeMap::new()).unwrap();
            idx.run_mut(
                &GraphIndex::usage_stat_create_ddl("usage_stat"),
                BTreeMap::new(),
            )
            .unwrap();
            idx.write_schema_version(1).unwrap();
        }

        let idx2 = GraphIndex::open(&dir, ".ckg").expect("reopen");
        // The staged row was adopted into `usage_stat` — nothing lost.
        let series = idx2.usage_turn_series("s1").unwrap();
        assert_eq!(series.len(), 1, "staged rows recovered without loss");
        assert_eq!(series[0].msg_id, "m1");
        assert_eq!(
            series[0].tokens.get("input"),
            Some(100),
            "token counts preserved through recovery"
        );
        assert_eq!(series[0].origin, "session");
        // The stage was consumed (renamed over `usage_stat`), leaving no leftover.
        assert!(
            !idx2
                .existing_relations()
                .unwrap()
                .contains("usage_stat_v24"),
            "the stage is gone after promotion"
        );
        assert!(idx2.usage_stat_has_origin().unwrap());

        drop(idx2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_per_tool_and_turn_series_join_tool_results_to_the_following_turn() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-join-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        // Turn 1 (the tool-calling message) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "t1".to_string(),
                model: Some("m".to_string()),
                in_tok: 10,
                out_tok: 5,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            100,
        )
        .unwrap();
        // ... then its tool result arrives (chars attributed to the NEXT turn) ...
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult {
                tool: Some("Read".to_string()),
                chars: 500,
            },
            110,
        )
        .unwrap();
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult {
                tool: Some("Read".to_string()),
                chars: 300,
            },
            111,
        )
        .unwrap();
        // ... then turn 2 (which "saw" that tool output as its input context).
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "t2".to_string(),
                model: Some("m".to_string()),
                in_tok: 800,
                out_tok: 20,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            120,
        )
        .unwrap();

        let per_tool = idx.usage_per_tool("s1").unwrap();
        assert_eq!(per_tool, vec![("Read".to_string(), 800)]);

        let series = idx.usage_turn_series("s1").unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].msg_id, "t1");
        assert_eq!(
            series[0].tool_chars, 0,
            "no tool results before the first turn"
        );
        assert_eq!(series[1].msg_id, "t2");
        assert_eq!(
            series[1].tool_chars, 800,
            "both Read results attributed to the turn that followed them"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_per_tool_buckets_unjoined_results_as_unknown() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-unknown-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult {
                tool: None,
                chars: 42,
            },
            100,
        )
        .unwrap();
        assert_eq!(
            idx.usage_per_tool("s1").unwrap(),
            vec![("unknown".to_string(), 42)]
        );
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_all_sessions_reports_totals_cache_ratio_and_est_only() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-allsess-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        idx.record_usage_event(
            "c1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m1".to_string(),
                model: None,
                in_tok: 100,
                out_tok: 10,
                cache_read: 50,
                cache_make: 0,
                origin: "session".to_string(),
            },
            100,
        )
        .unwrap();
        // Two modeled turns (opus outweighs sonnet in tokens) plus a
        // `<synthetic>` turn that must NOT surface in `models`.
        idx.record_usage_event(
            "c1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m2".to_string(),
                model: Some("claude-sonnet-5".to_string()),
                in_tok: 5,
                out_tok: 5,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            110,
        )
        .unwrap();
        idx.record_usage_event(
            "c1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m3".to_string(),
                model: Some("claude-opus-4-8".to_string()),
                in_tok: 200,
                out_tok: 20,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            120,
        )
        .unwrap();
        idx.record_usage_event(
            "c1",
            "claude",
            &UsageEvent::Turn {
                msg_id: "m4".to_string(),
                model: Some("<synthetic>".to_string()),
                in_tok: 1,
                out_tok: 1,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            130,
        )
        .unwrap();
        idx.record_usage_event(
            "o1",
            "opencode",
            &UsageEvent::ToolResult {
                tool: Some("edit".to_string()),
                chars: 20,
            },
            200,
        )
        .unwrap();
        // V24 Phase E: `est_only` keys off the token totals, not the agent.
        // An OpenCode session WITH real Turn tokens (Phase F plugin-reported)
        // is exact; a Claude session that only produced tool-result chars (no
        // turn ever landed) is est-only despite being Claude.
        idx.record_usage_event(
            "o2",
            "opencode",
            &UsageEvent::Turn {
                msg_id: "om1".to_string(),
                model: Some("anthropic/claude-opus-4-8".to_string()),
                in_tok: 42,
                out_tok: 7,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            210,
        )
        .unwrap();
        idx.record_usage_event(
            "c2",
            "claude",
            &UsageEvent::ToolResult {
                tool: Some("read".to_string()),
                chars: 12,
            },
            220,
        )
        .unwrap();
        // V24 code-review: a Claude session whose ONLY turn is a zero-token line
        // (an API-error turn — `parse_usage_line`'s tolerant default) is NOT
        // est-only. A recorded turn means exact accounting exists, even at zero
        // tokens; the old summed-totals rule mis-flagged this as est.
        idx.record_usage_event(
            "c3",
            "claude",
            &UsageEvent::Turn {
                msg_id: "cz1".to_string(),
                model: None,
                in_tok: 0,
                out_tok: 0,
                cache_read: 0,
                cache_make: 0,
                origin: "session".to_string(),
            },
            230,
        )
        .unwrap();

        let rows = idx.usage_all_sessions().unwrap();
        let claude = rows
            .iter()
            .find(|r| r.session_id == "c1")
            .expect("c1 present");
        assert!(!claude.est_only, "claude sessions carry exact usage");
        assert_eq!(claude.totals.get("input"), Some(306));
        // cache_read / (cache_read + in_tok) = 50 / 356.
        assert!((claude.cache_hit_ratio - (50.0 / 356.0)).abs() < 1e-9);
        assert_eq!(
            claude.models,
            vec!["claude-opus-4-8".to_string(), "claude-sonnet-5".to_string()],
            "models rank by tokens desc; model-less and <synthetic> turns excluded"
        );

        let opencode = rows
            .iter()
            .find(|r| r.session_id == "o1")
            .expect("o1 present");
        assert!(
            opencode.est_only,
            "a tool_result-only session has zero token totals ⇒ est-only"
        );
        // OpenCode DECLARES all four categories (V40 Phase G: it records
        // turns even though it reports no quota), so a session of its with no
        // turn rows still answers `input: 0` — a declared category at zero,
        // which is a different statement from an undeclared one being absent.
        assert_eq!(
            opencode.totals.get("input"),
            Some(0),
            "a tool_result-only session has zero token totals"
        );
        assert_eq!(opencode.tool_chars, 20);
        assert_eq!(
            opencode.cache_hit_ratio, 0.0,
            "no denominator ⇒ 0.0, not NaN"
        );
        assert!(
            opencode.models.is_empty(),
            "tool_result-only session has no models"
        );

        // OpenCode WITH real tokens → NOT est-only (agent name is irrelevant).
        let oc_tokens = rows
            .iter()
            .find(|r| r.session_id == "o2")
            .expect("o2 present");
        assert!(
            !oc_tokens.est_only,
            "an OpenCode session with real Turn tokens is exact"
        );
        assert_eq!(oc_tokens.totals.get("input"), Some(42));
        // Claude with NO turn rows (tool-result chars only) → est-only (derived
        // from turn presence, not agent).
        let claude_notoks = rows
            .iter()
            .find(|r| r.session_id == "c2")
            .expect("c2 present");
        assert!(
            claude_notoks.est_only,
            "a Claude session with no turn rows is est-only"
        );
        // Claude WITH a zero-token turn → NOT est-only: the recorded turn is
        // exact accounting even though every token count is zero.
        let claude_ztok = rows
            .iter()
            .find(|r| r.session_id == "c3")
            .expect("c3 present");
        assert!(
            !claude_ztok.est_only,
            "a recorded zero-token turn is exact, not est"
        );
        assert_eq!(
            claude_ztok.totals.get("input"),
            Some(0),
            "the turn carries zero tokens"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_ring_prunes_beyond_the_per_session_cap() {
        let dir = std::env::temp_dir().join(format!("ckg-usage-ring-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");

        let total = MAX_USAGE_PER_SESSION + 5;
        for i in 0..total {
            idx.record_usage_event(
                "s1",
                "claude",
                &UsageEvent::ToolResult {
                    tool: Some("Bash".to_string()),
                    chars: 1,
                },
                100 + i,
            )
            .unwrap();
        }
        let per_tool = idx.usage_per_tool("s1").unwrap();
        let (_, chars) = per_tool
            .into_iter()
            .find(|(t, _)| t == "Bash")
            .expect("Bash present");
        assert_eq!(
            chars, MAX_USAGE_PER_SESSION as u64,
            "the ring keeps exactly the cap's worth of rows (1 char each), not `total`"
        );

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_tool_ranking_reports_chars_and_call_counts() {
        let dir = std::env::temp_dir().join(format!("ckg-toolrank-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        for chars in [100, 200] {
            idx.record_usage_event(
                "s1",
                "claude",
                &UsageEvent::ToolResult {
                    tool: Some("Read".to_string()),
                    chars,
                },
                100,
            )
            .unwrap();
        }
        idx.record_usage_event(
            "s1",
            "claude",
            &UsageEvent::ToolResult {
                tool: Some("Bash".to_string()),
                chars: 50,
            },
            100,
        )
        .unwrap();
        let ranking = idx.usage_tool_ranking("s1").unwrap();
        assert_eq!(
            ranking,
            vec![("Read".to_string(), 300, 2), ("Bash".to_string(), 50, 1)]
        );
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── V24 Phase B: session drill-in query surface ────────────────────────

    /// Seed one turn with the given model/tokens/origin — the drill-in tests'
    /// shared helper.
    fn seed_turn(
        idx: &GraphIndex,
        sid: &str,
        msg: &str,
        model: Option<&str>,
        toks: (u32, u32, u32, u32),
        origin: &str,
        ts: i64,
    ) {
        idx.record_usage_event(
            sid,
            "claude",
            &UsageEvent::Turn {
                msg_id: msg.to_string(),
                model: model.map(str::to_string),
                in_tok: toks.0,
                out_tok: toks.1,
                cache_read: toks.2,
                cache_make: toks.3,
                origin: origin.to_string(),
            },
            ts,
        )
        .unwrap();
    }

    #[test]
    fn usage_session_model_totals_orders_by_tokens_and_splits_origin() {
        let dir = std::env::temp_dir().join(format!("ckg-permodel-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        // model-a: a Session turn (150 tok) + an Agent turn (10 tok) = 160.
        seed_turn(
            &idx,
            "s1",
            "m1",
            Some("model-a"),
            (100, 20, 30, 0),
            "session",
            100,
        );
        seed_turn(
            &idx,
            "s1",
            "m2",
            Some("model-a"),
            (10, 0, 0, 0),
            "agent",
            110,
        );
        // model-b: one Session turn (5 tok) — fewer tokens, ranks after model-a.
        seed_turn(
            &idx,
            "s1",
            "m3",
            Some("model-b"),
            (5, 0, 0, 0),
            "session",
            120,
        );
        // `<synthetic>` and no-model rows are excluded (parity with
        // `usage_session_models`), even carrying large token counts.
        seed_turn(
            &idx,
            "s1",
            "m4",
            Some("<synthetic>"),
            (999, 0, 0, 0),
            "session",
            130,
        );
        seed_turn(
            &idx,
            "s1",
            "m5",
            None,
            (999, 0, 0, 0),
            "session",
            140,
        );

        let per_model = idx.usage_session_model_totals("s1").unwrap();
        assert_eq!(per_model.len(), 2, "synthetic + no-model rows are excluded");
        // Ordered by total tokens desc.
        assert_eq!(per_model[0].model, "model-a");
        assert_eq!(per_model[0].totals.get("input"), Some(110));
        assert_eq!(per_model[0].totals.get("output"), Some(20));
        assert_eq!(per_model[0].totals.get("cache_read"), Some(30));
        assert_eq!(per_model[0].totals.get("cache_write"), Some(0));
        assert_eq!(
            per_model[0].origins.get("session").copied(),
            Some(150),
            "the main-lane turn's 150 tok"
        );
        assert_eq!(
            per_model[0].origins.get("agent").copied(),
            Some(10),
            "the sub-agent turn's 10 tok"
        );
        assert_eq!(per_model[1].model, "model-b");
        assert_eq!(per_model[1].origins.get("session").copied(), Some(5));
        // **Absent, not zero**: model-b recorded no sub-agent turn, and the
        // closed `OriginSplit` this replaced would have reported `agent_tok:
        // 0` — a lane the data never spoke about.
        assert_eq!(per_model[1].origins.get("agent"), None);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_session_model_totals_sum_matches_session_totals() {
        // The Cost-card honesty invariant: with every turn carrying a real
        // model, the per-model totals sum back to the whole-session totals.
        let dir = std::env::temp_dir().join(format!("ckg-permodel-sum-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        seed_turn(
            &idx,
            "s1",
            "m1",
            Some("model-a"),
            (100, 20, 30, 5),
            "session",
            100,
        );
        seed_turn(
            &idx,
            "s1",
            "m2",
            Some("model-b"),
            (7, 3, 1, 0),
            "agent",
            110,
        );

        let per_model = idx.usage_session_model_totals("s1").unwrap();
        let mut summed: BTreeMap<String, u64> = BTreeMap::new();
        for m in &per_model {
            for id in m.totals.ids() {
                *summed.entry(id.to_string()).or_insert(0) +=
                    m.totals.get(id).expect("id came from ids()");
            }
        }
        let whole = idx.usage_session_totals("s1").unwrap();
        let whole_map: BTreeMap<String, u64> = whole
            .ids()
            .map(|id| (id.to_string(), whole.get(id).expect("id came from ids()")))
            .collect();
        assert_eq!(summed, whole_map);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_session_row_is_none_for_unknown_but_present_for_a_seeded_session() {
        let dir = std::env::temp_dir().join(format!("ckg-sessrow-{}", uuid::Uuid::new_v4()));
        let idx = GraphIndex::open(&dir, ".ckg").expect("open");
        assert!(
            idx.usage_session_row("nope").unwrap().is_none(),
            "unknown session → None"
        );

        seed_turn(
            &idx,
            "s1",
            "m1",
            Some("model-a"),
            (100, 20, 30, 0),
            "session",
            100,
        );
        let row = idx
            .usage_session_row("s1")
            .unwrap()
            .expect("seeded session has a row");
        assert_eq!(row.session_id, "s1");
        assert_eq!(row.agent, "claude");
        assert!(!row.est_only, "a claude session is not est-only");
        assert_eq!(row.totals, idx.usage_session_totals("s1").unwrap());
        assert_eq!(row.models, vec!["model-a".to_string()]);
        // Same row the whole-project scan would produce for this id.
        let from_all = idx
            .usage_all_sessions()
            .unwrap()
            .into_iter()
            .find(|r| r.session_id == "s1")
            .unwrap();
        assert_eq!(row, from_all);

        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
