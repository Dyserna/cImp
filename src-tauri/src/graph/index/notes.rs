//! V32 Phase C2 (locked decision 10) — the `mem_note` relation and **the one
//! quarantine filter**.
//!
//! # The invariant
//!
//! A quarantined (`tainted`) note must not reach any recall or injection path.
//! It is enforced at the storage layer, once: [`GraphIndex::mem_notes`] drops
//! quarantined rows, and every consumer inherits that — `context_notes`, the
//! compaction carry-over, the fact distiller (and therefore the launch-time
//! `fact_promotion_block`), the Memory UI's clean list. The review UI opts
//! *in*, through [`GraphIndex::mem_quarantined_notes`].
//!
//! # Why this is a module and not a comment (#47)
//!
//! The invariant has two halves and they need different mechanisms.
//!
//! **"Only `graph::index` may execute a Cozo script"** is already structural:
//! `GraphIndex::run`, `run_mut`, `put`, `with_write_txn` and the free `tx_run`
//! are private to `graph::index`, so no other module can reach the relation by
//! any spelling. Nothing here adds to that half.
//!
//! **"Only ONE query applies the filter"** was not. `graph/index.rs` is ~8,800
//! lines, "the second note query in this file is the bug" is not a property a
//! reviewer can hold in their head, and the test that claimed to hold it passed
//! **vacuously**: it lived in the very file it allowed and self-matched on its
//! own doc comment and its own search literal, so renaming the relation would
//! have left decision 10 unguarded with a green suite (V32 review, Part 4
//! item 1).
//!
//! This module is the answer to that half. Every statement naming the relation
//! lives here, in one short file, and the parent cannot add one without adding
//! it *here* — where a reviewer opening the file sees all of them at once. It
//! is encapsulation, not a compile error: `graph/index.rs` still owns the
//! executor, and a CozoScript is a `&str`, so privacy cannot stop the parent
//! from writing a raw query. That residue is what
//! `tests::note_queries_live_only_in_this_module` backstops — the same idea as
//! the retired scan, but over a few hundred lines instead of 8,800, and with
//! the three self-guards the old one lacked.
//!
//! Deletes live here too, as the `RM_*` script constants `graph/index.rs`'s
//! transaction paths use. They cannot leak a quarantined note (they remove
//! rows), but keeping their text here is what lets the scan assert "**no**
//! query outside this module" instead of maintaining an exception list.
//!
//! **House rule for this file:** never write the relation's datalog atom form
//! (a `*` immediately followed by the relation name and a `{`) inside a comment
//! or a string that is not a real query. A comment that satisfies the "the
//! guarded thing still exists" self-guard is precisely the vacuity bug this
//! module replaces — and #48 found the rule already broken, by `atom()`'s own
//! doc comment, one commit after it was written. The rule is therefore
//! **executable** now: the scan's fourth self-guard fails on any match that
//! sits behind a `//` on its line, so prose satisfying the count is a red test
//! rather than a silent hole. Write "the atom form" in words instead.
//!
//! # The two blind spots, and what happened to them (#48)
//!
//! Both were recorded here as "stated, not fixed" when the module shipped. They
//! are closed now, by different means, and the residue that remains is named.
//!
//! - **A query whose relation name is interpolated** was invisible to a literal
//!   scan: `mem_note_row_count` built its atom with a `format!` over a `name`
//!   parameter, so no literal atom appeared in the source at all. That call was
//!   migration-only and its blast radius was bounded by the module boundary
//!   rather than by the scan — which is exactly the argument that would have to
//!   be re-made, correctly, for every future one.
//!
//!   Fixed by **removing the interpolation**: [`GraphIndex::mem_note_stage_row_count`]
//!   spells the stage relation out, and `tests::no_interpolated_relation_atom`
//!   makes a new one a red test. The residue: an interpolated atom is only
//!   banned *here*. `graph/index.rs` legitimately has two (over `usage_stat` and
//!   the session relations) and a blanket ban would be wrong there, so a future
//!   parameterized read in the parent whose parameter happens to be this
//!   relation is still covered by the module boundary alone — 8,800 lines of it.
//!   Narrowing that is a type problem (a relation newtype), not a scan problem.
//!
//! - **Statements that name the relation without an atom.** `graph/index.rs`'s
//!   `#[cfg(test)] mod tests` ran four — a `::remove`, a pre-C2 `:create`, and
//!   two `:put`s — to build the fixtures the migration tests need. None could
//!   *read* a row (they are DDL and writes), so none could bypass the filter,
//!   which is why the scan was green with them present.
//!
//!   Fixed by **moving the text here**: they are the `FIXTURE_*` constants at
//!   the bottom of this file, and the parent's tests reference them. The module
//!   docs' claim — *"every statement naming the relation lives here, where a
//!   reviewer opening the file sees all of them at once"* — is now literally
//!   true rather than true-except-for-four. The residue: no scan enforces it,
//!   because the only pattern that would (the bare relation IDENTIFIER) also
//!   matches the dozen legitimate prose mentions across `index.rs`,
//!   `memory.rs`, `schema.rs` and `toolclass.rs`, and separating those needs
//!   the comment heuristic `docs/MAINTENANCE.md` § *Cross-module invariants*
//!   bans. Stated rather than half-enforced.

use std::collections::{BTreeMap, HashSet};

use cozo::{DataValue, Num, ScriptMutability};

use crate::error::{AppError, AppResult};
use crate::graph::memory::MemNote;

use super::{cell_bool, cell_i64, cell_str, GraphIndex};

// ── Delete scripts used by `graph/index.rs`'s transaction cascades ─────────
//
// `session_id` is a VALUE column on this relation (the key is `note_id`), so
// unlike the session-keyed relations there is no prefix bind to inline and the
// post-filter stays. Exported as constants rather than moved wholesale because
// each is one step of a multi-relation cascade that must stay in its
// transaction.

/// Remove every note belonging to session `$sid` — `mem_clear(Some(sid))`.
pub(super) const RM_BY_SESSION: &str =
    "?[note_id] := *mem_note{note_id, session_id}, session_id == $sid\n:rm mem_note {note_id}";

/// Remove every note in the project — `mem_clear(None)`.
pub(super) const RM_ALL: &str = "?[note_id] := *mem_note{note_id}\n:rm mem_note {note_id}";

/// Remove session `$sid`'s UNPINNED notes — the count-cap eviction and the
/// retention sweep. Pinned notes survive project-wide by definition: they
/// outlive the session that wrote them.
pub(super) const RM_UNPINNED_BY_SESSION: &str =
    "?[note_id] := *mem_note{note_id, session_id, pinned}, session_id == $sid, pinned == false\n\
     :rm mem_note {note_id}";

impl GraphIndex {
    /// Ensure the relation exists. Called from
    /// [`GraphIndex::ensure_memory_relations`] with the relation set it already
    /// read, so this costs no extra query.
    ///
    /// V32 Phase C2: same posture as `usage_stat` — additive, survives a graph
    /// rebuild, and its DDL is shared with the migration stage so the two
    /// shapes cannot drift.
    pub(super) fn ensure_mem_note_relation(&self, existing: &HashSet<String>) -> AppResult<()> {
        if !existing.contains("mem_note") {
            self.run_mut(&Self::mem_note_create_ddl("mem_note"), BTreeMap::new())?;
        }
        Ok(())
    }

    /// The `mem_note` relation's `:create` DDL, parameterized by relation `name`
    /// so the live relation ([`Self::ensure_memory_relations`]) and the V32
    /// Phase C2 migration stage ([`Self::migrate_mem_note_tainted`]) share one
    /// source of truth — the C2 shape, carrying the `tainted` column.
    ///
    /// `tainted` is declared `default false` so a `:put` that predates the
    /// column (or a future partial write) lands as *not quarantined* rather than
    /// failing — the same honest-default posture as `ref.confidence`. Every
    /// writer in this file still passes it explicitly; the default is the
    /// backstop, not the contract.
    pub(super) fn mem_note_create_ddl(name: &str) -> String {
        format!(
            ":create {name} {{note_id: String => session_id: String, text: String, ts_ms: Int, \
                pinned: Bool, tainted: Bool default false}}"
        )
    }

    /// The migration stage relation used by [`Self::migrate_mem_note_tainted`].
    /// Same contract as [`Self::USAGE_STAT_STAGE`]: a fully-populated new-shape
    /// copy built atomically before the old `mem_note` is ever dropped, so its
    /// presence on open always means "a prior migration was interrupted
    /// mid-swap; adopt me".
    pub(super) const MEM_NOTE_STAGE: &'static str = "mem_note_v32";

    /// V32 Phase C2: add the `tainted` column to a pre-C2 `mem_note` relation,
    /// defaulting existing rows to **not quarantined**.
    ///
    /// Defaulting old rows to `false` is the only defensible choice and is
    /// deliberately NOT a security claim: pre-V32 notes are unauditable (locked
    /// decision 10 says so), and marking every one of them quarantined would
    /// dump a user's entire note history into a review queue they cannot
    /// meaningfully triage. The compensating control for unauditable memory is
    /// the delivery-time spotlighting envelope, which wraps clean notes too.
    ///
    /// Mechanically identical to [`Self::migrate_usage_stat_origin`] — see that
    /// method for why the crash-safe stage-and-swap is shaped this way (CozoDB
    /// autocommits each script, so a naive remove→create→put loses data if the
    /// process dies mid-sequence). Called from the writable [`Self::open`]
    /// migration path only; `open_existing` consumers are read-only and are
    /// protected instead by the [`super::GRAPH_SCHEMA_VERSION`] gate, which refuses a
    /// store whose `mem_note` has not been migrated yet.
    pub(super) fn migrate_mem_note_tainted(&self) -> AppResult<()> {
        let existing = self.existing_relations()?;
        // Recovery: a leftover stage means a prior migration was interrupted
        // after the stage was durably populated. Adopt the stage over whatever
        // `mem_note` currently is — never the reverse, so no notes are lost.
        if existing.contains(Self::MEM_NOTE_STAGE) {
            return self.promote_mem_note_stage();
        }
        if !existing.contains("mem_note") {
            return Ok(());
        }
        if self.relation_has_column("mem_note", "tainted")? {
            return Ok(());
        }
        let rows = self.run(
            "?[note_id, session_id, text, ts_ms, pinned] := \
                *mem_note{note_id, session_id, text, ts_ms, pinned}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let expected = rows.rows.len();
        let migrated: Vec<DataValue> = rows
            .rows
            .into_iter()
            .map(|mut r| {
                r.push(DataValue::Bool(false));
                DataValue::List(r)
            })
            .collect();
        if migrated.is_empty() {
            self.run_mut(
                &Self::mem_note_create_ddl(Self::MEM_NOTE_STAGE),
                BTreeMap::new(),
            )?;
        } else {
            let mut p = BTreeMap::new();
            p.insert("rows".to_string(), DataValue::List(migrated));
            self.run_mut(
                &format!(
                    "?[note_id, session_id, text, ts_ms, pinned, tainted] <- $rows\n{}",
                    Self::mem_note_create_ddl(Self::MEM_NOTE_STAGE)
                ),
                p,
            )?;
        }
        // Verify the stage captured every old row before dropping the original.
        let staged = self.mem_note_stage_row_count()?;
        if staged != expected {
            self.run_mut(&format!("::remove {}", Self::MEM_NOTE_STAGE), BTreeMap::new())?;
            return Err(AppError::Graph(format!(
                "mem_note migration stage captured {staged} of {expected} rows; aborting"
            )));
        }
        self.promote_mem_note_stage()
    }

    /// Promote a fully-populated [`Self::MEM_NOTE_STAGE`] to `mem_note`.
    /// Idempotent on retry, exactly like [`Self::promote_usage_stat_stage`].
    fn promote_mem_note_stage(&self) -> AppResult<()> {
        if self.existing_relations()?.contains("mem_note") {
            self.run_mut("::remove mem_note", BTreeMap::new())?;
        }
        self.run_mut(
            &format!("::rename {} -> mem_note", Self::MEM_NOTE_STAGE),
            BTreeMap::new(),
        )?;
        Ok(())
    }

    /// Row count of the migration STAGE relation, counted by its `note_id`
    /// primary key so CozoScript's set semantics cannot dedupe distinct rows
    /// into an undercount. Migration verification only.
    ///
    /// **The relation name is spelled out, not interpolated** (#48). The
    /// previous form took a `name` parameter and built its atom by interpolating
    /// that parameter with `format!`, which is a real note query the scan below
    /// cannot see — the first blind spot the module docs recorded. Nothing needed the
    /// parameter: the only caller passes [`Self::MEM_NOTE_STAGE`]. The literal
    /// deliberately does not match `atom()` (a name continuing past the
    /// relation name has neither `{` nor `[` next), which is the same property
    /// that keeps the stage out of the [`tests::NOTE_QUERIES`] floor.
    ///
    /// [`tests::the_stage_literal_matches_the_constant`] is what keeps the two
    /// spellings from drifting.
    fn mem_note_stage_row_count(&self) -> AppResult<usize> {
        let rows = self.run(
            "?[note_id] := *mem_note_v32{note_id}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.len())
    }

    /// Record a note (a decision/fact) for a session.
    ///
    /// V32 Phase C2: `tainted` marks the note **quarantined** — stored, but
    /// invisible to every read path until a human promotes it
    /// ([`Self::mem_promote_note`]). It is decided by the loopback taint latch
    /// (`Latch::proxy_gate`) and threaded here through `run_graph_tool` →
    /// `dispatch_recorded` → `run_tool`; nothing in this layer infers it, so a
    /// caller that forgets it writes a clean note — which is why the parameter
    /// is a `WriteTaint` at every layer above rather than a bare `bool`.
    pub fn mem_add_note(
        &self,
        note_id: &str,
        session_id: &str,
        text: &str,
        ts_ms: i64,
        pinned: bool,
        tainted: bool,
    ) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), DataValue::Str(note_id.into()));
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        p.insert("text".to_string(), DataValue::Str(text.into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(ts_ms)));
        p.insert("pin".to_string(), DataValue::Bool(pinned));
        p.insert("taint".to_string(), DataValue::Bool(tainted));
        self.run_mut(
            "?[note_id, session_id, text, ts_ms, pinned, tainted] <- \
                [[$nid, $sid, $text, $ts, $pin, $taint]]\n\
             :put mem_note {note_id => session_id, text, ts_ms, pinned, tainted}",
            p,
        )?;
        Ok(())
    }

    /// Set/clear the pinned flag on a note.
    pub fn mem_set_note_pinned(&self, note_id: &str, pinned: bool) -> AppResult<()> {
        self.rewrite_note(note_id, |n| n.pinned = pinned)
    }

    /// V32 Phase C2: release a quarantined note into normal memory. Clears
    /// `tainted` only — the pinned state is preserved, because the model's
    /// `pin: true` was a statement about the note's *scope*, and re-deciding it
    /// on the user's behalf would either lose a durable finding or silently
    /// promote a session note to project-wide.
    pub fn mem_promote_note(&self, note_id: &str) -> AppResult<()> {
        self.rewrite_note(note_id, |n| n.tainted = false)
    }

    /// V32 Phase C2: permanently delete one note (the quarantine review's
    /// DISCARD action). Tolerant of a missing id, like every other single-row
    /// mutation here — a stale UI row must not raise an error toast.
    pub fn mem_delete_note(&self, note_id: &str) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), DataValue::Str(note_id.into()));
        self.run_mut(
            "?[note_id] := *mem_note{note_id}, note_id == $nid\n:rm mem_note {note_id}",
            p,
        )?;
        Ok(())
    }

    /// Read-modify-write one note's row, applying `mutate` to the in-memory
    /// copy — the same shape (and the same tolerant-missing-id posture) as
    /// [`Self::rewrite_fact`]. Reading every column back and writing it whole is
    /// what keeps a future column from being silently dropped by a partial
    /// `:put`; `tainted` was exactly such a column when C2 added it.
    fn rewrite_note(&self, note_id: &str, mutate: impl FnOnce(&mut MemNote)) -> AppResult<()> {
        let mut p = BTreeMap::new();
        p.insert("nid".to_string(), DataValue::Str(note_id.into()));
        let rows = self.run(
            "?[session_id, text, ts_ms, pinned, tainted] := \
                *mem_note{note_id, session_id, text, ts_ms, pinned, tainted}, note_id == $nid",
            p.clone(),
            ScriptMutability::Immutable,
        )?;
        let Some(r) = rows.rows.first() else {
            return Ok(());
        };
        let mut note = MemNote {
            note_id: note_id.to_string(),
            session_id: cell_str(r, 0),
            text: cell_str(r, 1),
            ts_ms: cell_i64(r, 2),
            pinned: cell_bool(r, 3),
            tainted: cell_bool(r, 4),
        };
        mutate(&mut note);
        p.insert(
            "sid".to_string(),
            DataValue::Str(note.session_id.as_str().into()),
        );
        p.insert("text".to_string(), DataValue::Str(note.text.as_str().into()));
        p.insert("ts".to_string(), DataValue::Num(Num::Int(note.ts_ms)));
        p.insert("pin".to_string(), DataValue::Bool(note.pinned));
        p.insert("taint".to_string(), DataValue::Bool(note.tainted));
        self.run_mut(
            "?[note_id, session_id, text, ts_ms, pinned, tainted] <- \
                [[$nid, $sid, $text, $ts, $pin, $taint]]\n\
             :put mem_note {note_id => session_id, text, ts_ms, pinned, tainted}",
            p,
        )?;
        Ok(())
    }

    /// A session's notes plus every pinned note in the project, pinned first
    /// then newest.
    ///
    /// **V32 Phase C2: quarantined (`tainted`) notes are excluded here**, at the
    /// storage layer, rather than in each caller. That is deliberate — this one
    /// method backs *every* consumer of session notes (`context_notes`, the
    /// compaction carry-over, the fact distiller, the Memory UI), and locked
    /// decision 10's invariant is "no read path leaks a tainted note except the
    /// review UI". A filter each caller had to remember to apply would be one
    /// forgotten call site away from re-opening the persistence channel; the
    /// review UI opts *in* instead, via [`Self::mem_quarantined_notes`].
    pub fn mem_notes(&self, session_id: &str) -> AppResult<Vec<MemNote>> {
        let mut p = BTreeMap::new();
        p.insert("sid".to_string(), DataValue::Str(session_id.into()));
        let rows = self.run(
            "?[note_id, session_id, text, ts_ms, pinned] := \
                *mem_note{note_id, session_id, text, ts_ms, pinned, tainted}, \
                tainted == false, (session_id == $sid or pinned == true)",
            p,
            ScriptMutability::Immutable,
        )?;
        let mut notes: Vec<MemNote> = rows
            .rows
            .iter()
            .map(|r| MemNote {
                note_id: cell_str(r, 0),
                session_id: cell_str(r, 1),
                text: cell_str(r, 2),
                ts_ms: cell_i64(r, 3),
                pinned: cell_bool(r, 4),
                tainted: false,
            })
            .collect();
        notes.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.ts_ms.cmp(&a.ts_ms)));
        Ok(notes)
    }

    /// V32 Phase C2: every quarantined note in the project, newest first.
    ///
    /// Project-wide and session-independent on purpose: a quarantined note's
    /// own session is by definition a contaminated one, often already finished,
    /// so scoping the review queue to "the current session" would hide exactly
    /// the notes that need a decision. This is the one read path allowed to
    /// return tainted rows, and its only consumer is the Memory UI
    /// (`GraphService::memory_snapshot`).
    pub fn mem_quarantined_notes(&self) -> AppResult<Vec<MemNote>> {
        let rows = self.run(
            "?[note_id, session_id, text, ts_ms, pinned] := \
                *mem_note{note_id, session_id, text, ts_ms, pinned, tainted}, tainted == true",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        let mut notes: Vec<MemNote> = rows
            .rows
            .iter()
            .map(|r| MemNote {
                note_id: cell_str(r, 0),
                session_id: cell_str(r, 1),
                text: cell_str(r, 2),
                ts_ms: cell_i64(r, 3),
                pinned: cell_bool(r, 4),
                tainted: true,
            })
            .collect();
        notes.sort_by_key(|n| std::cmp::Reverse(n.ts_ms));
        Ok(notes)
    }

    /// How many notes are quarantined project-wide. Feeds the `context_notes`
    /// tool's count-only footer — the model is told *that* its write landed in
    /// review, never the withheld text, so a compromised session cannot use the
    /// quarantine as a side channel to read back what it planted.
    pub fn mem_quarantined_count(&self) -> AppResult<usize> {
        let rows = self.run(
            "?[note_id] := *mem_note{note_id, tainted}, tainted == true",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        )?;
        Ok(rows.rows.len())
    }
}

// ── Migration-test fixture scripts (#48) ───────────────────────────────────
//
// `graph/index.rs`'s migration tests need to build a PRE-C2 store and an
// interrupted-swap store, which means running DDL and writes that name the
// relation. They lived in that file, which made the module docs' "every
// statement naming the relation lives here" claim false by four.
//
// None of them can read a row, so none could ever bypass the quarantine filter
// — moving them buys no enforcement. What it buys is the property the module
// exists for: a reviewer who opens this file sees every statement that names
// the relation, with nothing to remember about a second location.

/// Drop the live relation, so a test can re-create it in the pre-C2 shape.
#[cfg(test)]
pub(super) const FIXTURE_DROP: &str = "::remove mem_note";

/// The relation's shape BEFORE Phase C2 — no `tainted` column. Deliberately not
/// derived from [`GraphIndex::mem_note_create_ddl`]: it is the historical shape
/// the migration must cope with, and a fixture that tracked the current DDL
/// would stop testing the migration the day the DDL changed again.
#[cfg(test)]
pub(super) const FIXTURE_CREATE_PRE_C2: &str =
    ":create mem_note {note_id: String => session_id: String, text: String, ts_ms: Int, \
     pinned: Bool}";

/// Insert `$rows` in the pre-C2 column set.
#[cfg(test)]
pub(super) const FIXTURE_PUT_PRE_C2: &str = "?[note_id, session_id, text, ts_ms, pinned] <- $rows\n\
     :put mem_note {note_id => session_id, text, ts_ms, pinned}";

/// Insert `$rows` into the migration stage, in the C2 column set — what an
/// interrupted swap leaves behind for the recovery branch to adopt.
#[cfg(test)]
pub(super) const FIXTURE_PUT_STAGE: &str =
    "?[note_id, session_id, text, ts_ms, pinned, tainted] <- $rows\n\
     :put mem_note_v32 {note_id => session_id, text, ts_ms, pinned, tainted}";

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// This module's own file — the one place in `src/` a note query may
    /// appear. Slash-separated, matching what [`source_files`] produces.
    const SELF: &str = "graph/index/notes.rs";

    /// The source tree, resolved from the manifest rather than the process cwd
    /// so the scan gives the same answer from any working directory (verified
    /// by running the suite from `C:\`).
    fn src_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    /// Every `.rs` file under `src/`, as `(relative-slash-path, contents)`.
    /// Panics on a missing or unreadable tree: a scan that silently returns
    /// nothing is indistinguishable from a green suite.
    fn source_files() -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
            for e in entries.flatten() {
                let p = e.path();
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
        let root = src_root();
        assert!(
            root.is_dir(),
            "the source tree is missing at {} — this scan cannot run",
            root.display()
        );
        let mut out = Vec::new();
        walk(&root, &root, &mut out);
        out.sort();
        out
    }

    /// A datalog atom over the note relation, in **either** CozoScript spelling:
    /// the named form (`{a, b}`) this codebase uses and the positional form
    /// (`[a, b]`) it does not, with whitespace tolerated between every part.
    ///
    /// The retired version of this scan matched a fixed star-name-brace string,
    /// so both a space and the positional form evaded it entirely. The trailing
    /// delimiter is required, which is also what keeps the stage relation
    /// (`mem_note_v32`) and any future suffixed sibling from matching here: a
    /// name that continues past the relation name has neither `{` nor `[` next.
    ///
    /// This doc comment used to spell the retired pattern out, in breach of the
    /// file's house rule — and that one comment was enough to satisfy
    /// [`note_queries_live_only_in_this_module`]'s third self-guard all by
    /// itself, so renaming the relation left the suite fully green with **zero**
    /// production queries matched (#48). Guard 4 below now makes that shape a
    /// test failure instead of a comment nobody re-reads.
    fn atom() -> regex::Regex {
        regex::Regex::new(r"\*\s*mem_note\s*[\{\[]").expect("a static pattern compiles")
    }

    /// A datalog atom whose relation name is a `format!` placeholder rather
    /// than a literal — the shape [`atom`] cannot see, and the first of the two
    /// blind spots the module docs recorded (#48).
    ///
    /// Deliberately never spelled out in prose anywhere in this file: the house
    /// rule that governs the relation's atom form governs this shape for
    /// exactly the same reason, and unlike guard 4's subject this guard scans
    /// EVERY match in the file — including one sitting in its own doc comment.
    ///
    /// Scoped to `SELF` on purpose. `graph/index.rs` has two legitimate
    /// interpolated atoms over other relations (`usage_stat`'s row count, the
    /// session migration's read), and banning the shape there would be wrong.
    /// Banning it *here* is not: this file has exactly one relation to talk
    /// about, so an interpolated name in it is either this relation or a
    /// mistake, and both want the author to stop and write the name out.
    fn interpolated_atom() -> regex::Regex {
        regex::Regex::new(r"\*\s*\{\w+\}").expect("a static pattern compiles")
    }

    /// The text preceding byte offset `at` on its own line, plus that line's
    /// 1-based number — the two facts guard 4 reports a violation with.
    fn line_prefix(text: &str, at: usize) -> (usize, &str) {
        let bol = text[..at].rfind('\n').map_or(0, |i| i + 1);
        (text[..bol].matches('\n').count() + 1, &text[bol..at])
    }

    /// Every real note query in this module, counted by hand so a deletion
    /// fails here instead of thinning the guarded set quietly (#48).
    ///
    /// MAINTENANCE.md's § *Cross-module invariants* requires "a per-file floor
    /// for every known site" of any surviving scan, and this one shipped
    /// without it: `mine > 0` alone tolerated eight of the nine vanishing. The
    /// nine, in file order: `RM_BY_SESSION`, `RM_ALL`,
    /// `RM_UNPINNED_BY_SESSION`, the migration's read, `mem_delete_note`,
    /// `rewrite_note`'s read, `mem_notes`, `mem_quarantined_notes`,
    /// `mem_quarantined_count`.
    ///
    /// Raise it when a query is added; lowering it is a decision that belongs
    /// in a commit message, which is the point of it being a constant.
    const NOTE_QUERIES: usize = 9;

    /// The cross-module half of locked decision 10, as a backstop to the module
    /// boundary above: no file outside this one queries the note relation, so
    /// the quarantine filter in `mem_notes` cannot be bypassed by a reader that
    /// writes its own datalog.
    ///
    /// Four self-guards. Three were added by #47, and #48 added the fourth
    /// after the third turned out to be satisfiable by prose one commit later:
    ///
    /// 1. **`SELF` is excluded**, so this file's own regex and prose cannot
    ///    satisfy the assertion the way `graph/index.rs` satisfied the old one.
    /// 2. **The scan must find its own file**, so a rename or a move of this
    ///    module fails here instead of silently scanning nothing.
    /// 3. **The guarded thing must still exist**: [`NOTE_QUERIES`] real queries
    ///    left in this module, not merely one. Renaming the relation (say to a
    ///    `_v2`) makes every atom vanish everywhere; without this the scan would
    ///    then pass while guarding nothing at all, which is exactly how the old
    ///    one would have failed. Note what this guard deliberately does NOT try
    ///    to check: that the filter still *works*. That is behaviour, and
    ///    behaviour is pinned by a behavioural test — see
    ///    `quarantined_notes_are_hidden_from_reads_until_promoted` in
    ///    `graph/index.rs`. A source scan asserting that its own file contains
    ///    some substring is satisfied by its own error message.
    /// 4. **Every match here must be a real query** — the file's house rule,
    ///    made executable (#48). Guard 3 counts occurrences and cannot tell
    ///    prose from code; when it shipped, `atom()`'s own doc comment spelled
    ///    the retired pattern out and satisfied guard 3 on its own, so renaming
    ///    the relation identifier left the whole suite green with zero
    ///    production queries matched. A match sitting behind a `//` on its line
    ///    now fails.
    ///
    ///    **Why this is not the banned line heuristic** (MAINTENANCE.md §
    ///    *Cross-module invariants*). The ban is on heuristics whose wrong
    ///    answer *weakens* the invariant: the retired `in_comment` read a real
    ///    hit as a comment and SKIPPED it, so an offender went unreported. This
    ///    one only ever moves in the failing direction — it recognizes a subset
    ///    of comment placements and turns each into a red test. A placement it
    ///    does not recognize (a block comment, an atom inside a non-query
    ///    string) leaves the scan exactly where it stands today, covered by the
    ///    house rule as before. It answers "is there a line-comment opener
    ///    before this match", not "is this byte inside a comment"; the second is
    ///    the parsing question, and nothing here asks it.
    #[test]
    fn note_queries_live_only_in_this_module() {
        let files = source_files();
        let re = atom();
        let mut mine: Option<usize> = None;
        let mut prose: Vec<String> = Vec::new();
        let mut offenders: Vec<String> = Vec::new();
        for (rel, text) in &files {
            let hits = re.find_iter(text).count();
            if rel == SELF {
                mine = Some(hits);
                for m in re.find_iter(text) {
                    let (line, before) = line_prefix(text, m.start());
                    if before.contains("//") {
                        prose.push(format!("{rel}:{line}"));
                    }
                }
            } else if hits > 0 {
                offenders.push(format!("{rel} ({hits})"));
            }
        }

        let mine = mine.unwrap_or_else(|| {
            panic!(
                "the scan did not find its own source at `{SELF}` — this module moved and the \
                 scan is now watching nothing. Point SELF at the new path."
            )
        });
        assert!(
            prose.is_empty(),
            "HOUSE RULE (see this file's module docs) — the note relation's atom form is written \
             behind a `//` at: {prose:?}\n\n\
             A comment satisfies the count guard below without guarding anything, which is how \
             `atom()`'s own doc comment kept this whole scan green through a relation rename \
             (#48). Say \"the atom form\" in words instead."
        );
        assert!(
            mine >= NOTE_QUERIES,
            "expected at least {NOTE_QUERIES} note queries in `{SELF}`, found {mine} — if the \
             relation was renamed, update the pattern in `atom()` and this message; if a query \
             was deliberately removed, lower `NOTE_QUERIES` in the same commit. Until then \
             locked decision 10's read exclusion is guarded over a thinner set than it was, or \
             not at all."
        );
        assert!(
            offenders.is_empty(),
            "MEMORY QUARANTINE INVARIANT (V32 locked decision 10) — these files query the note \
             relation directly: {offenders:?}\n\n\
             A quarantined note is one a contaminated conversation wrote; a reader that writes \
             its own datalog gets the tainted rows too, and no unit test of that call site would \
             notice. Route the read through `GraphIndex::mem_notes` (quarantine-filtered) or \
             `GraphIndex::mem_quarantined_notes` (the review UI's explicit opt-in), or move the \
             query into `{SELF}` beside the others."
        );
    }

    /// Guard 5 (#48) — no note query in this file may hide its relation name
    /// behind a `format!` placeholder.
    ///
    /// This is the first blind spot the module docs used to record as "stated,
    /// not fixed": `mem_note_row_count` interpolated the relation name into its
    /// atom, and was therefore a real note query that
    /// [`note_queries_live_only_in_this_module`]'s pattern could not count,
    /// could not place, and could not have missed the disappearance of. The
    /// query is spelled out now; this keeps the next one from being written.
    ///
    /// Like guard 4 it only ever moves in the failing direction: the pattern
    /// recognizes the one Rust shape that produces an interpolated atom, and a
    /// shape it does not recognize leaves the scan exactly where it stands.
    #[test]
    fn no_interpolated_relation_atom() {
        let text = std::fs::read_to_string(src_root().join(SELF))
            .unwrap_or_else(|e| panic!("cannot read {SELF}: {e}"));
        let re = interpolated_atom();
        let hits: Vec<String> = re
            .find_iter(&text)
            .map(|m| {
                let (line, _) = line_prefix(&text, m.start());
                format!("{SELF}:{line} `{}`", m.as_str())
            })
            .collect();
        assert!(
            hits.is_empty(),
            "INTERPOLATED NOTE QUERY — a datalog atom whose relation name is a `format!` \
             placeholder is invisible to `note_queries_live_only_in_this_module`, so it is neither \
             counted by the floor nor found by a rename: {hits:?}\n\n\
             Write the relation name out. If the query really must be generic over two relations, \
             match on the name and hold one literal script per arm."
        );
    }

    /// The one place a relation name is written twice: [`GraphIndex::MEM_NOTE_STAGE`]
    /// and the literal inside `mem_note_stage_row_count`'s script. Removing the
    /// interpolation traded an invisible query for a possible drift; this is the
    /// trade's other half.
    #[test]
    fn the_stage_literal_matches_the_constant() {
        assert_eq!(
            super::GraphIndex::MEM_NOTE_STAGE,
            "mem_note_v32",
            "the stage constant moved — update the literal script in \
             `mem_note_stage_row_count` (and `FIXTURE_PUT_STAGE`) in the same commit"
        );
        assert!(
            super::FIXTURE_PUT_STAGE.contains(super::GraphIndex::MEM_NOTE_STAGE),
            "the stage fixture names a different relation than the constant"
        );
    }

    /// The migration fixtures the parent's tests run are here, and they are the
    /// shapes those tests actually need: the pre-C2 relation has no `tainted`
    /// column (that is the whole point of the migration test) and the stage
    /// fixture does.
    #[test]
    fn the_migration_fixtures_are_the_shapes_they_claim() {
        assert!(!super::FIXTURE_CREATE_PRE_C2.contains("tainted"));
        assert!(super::FIXTURE_CREATE_PRE_C2.contains("pinned: Bool"));
        assert!(!super::FIXTURE_PUT_PRE_C2.contains("tainted"));
        assert!(super::FIXTURE_PUT_STAGE.contains("tainted"));
        assert_eq!(super::FIXTURE_DROP, "::remove mem_note");
    }
}
