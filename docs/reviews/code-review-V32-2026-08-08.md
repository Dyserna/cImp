# Code review — V32 injection hardening, full re-review after the fix runs

**Range:** `033b36e~1..f31978c` (V32 phases A–H + both fix runs) — 79 non-doc files,
34,400 insertions / 2,172 deletions.
**Reviewed at:** `f31978c` (develop), 2026-08-08.
**Method:** 11 independent Opus 5 reviewers, one per containment surface, each
briefed with the locked decisions and the cross-module invariants, each tasked
with re-verifying the 2026-08-07 findings at HEAD *and* hunting regressions the
fixes introduced. Orchestrator verified every HIGH by hand before banking it.
Supersedes `code-review-V32-2026-08-07.md` where they disagree.

---

## Build state

| Check | Result |
|---|---|
| `cargo test` | **1781 passed, 0 failed**, 3 ignored |
| `npx vitest run` | **516 passed** (26 files) |
| `cargo clippy --all-targets` | **clean**, zero warnings |
| `cargo audit` | **16 vulnerabilities** (see S-1) |

Green, and green is not the same as contained — see below.

---

## Status ledger

Updated as findings are dispositioned. **FIXED** means closed in code with a
test; **DECLINED** means a recorded decision not to fix, with reasoning;
**PARTIAL** means the useful half landed and the residual is named.

| # | Finding | Status | Where |
|---|---|---|---|
| H-1 | C-1 open — `graph_struct_search`/`graph_repo_map` are TRUSTED source readers | **FIXED** | `0169d10` — demote both + strip `signature`; amendment 2026-08-08 (c) + decision 29 |
| H-2 | C-2 open — contamination clears on any newline-terminated byte | **FIXED** | `2c40136` — reset removed, not re-armed; amends decision 15 |
| H-3 | SSRF — `http:/host`, `http:host`, `http:\host` evade the screen | **FIXED** | `e5f3627` — scheme parsed not matched; 15,840-case generated corpus |
| H-4 | Every shipped rule defeated by invisible whitespace | **FIXED** | `5920c92` — rules + normalizer + corpus; 20/50 evading → 0/53 |
| H-5 | Update channel inert by construction (302 vs redirect ban) | **FIXED** | `5920c92` — `raw.githubusercontent.com` branch + the missing guard |
| H-6 | Bundle authenticated by TLS + repo-write only | **DECLINED** | locked decision 28 — key would live inside the blast radius |
| H-7 | A cloned repo's `opencode.json` is executed configuration | **OPEN** | largely V33 (OS containment); record as known-open meanwhile |
| H-8 | `/audit/run`'s taint gate is opt-in by the caller | **FIXED** | `80375a9` — `tab` required + `consumer` narrowed; both halves |
| H-9 | Forensic activity rows are model-evictable | **FIXED** | `d652171` — one retention lane per `Screen` (not the proposed pin); lane set derived from `Screen::ALL` |
| H-10 | A failed detection-status read renders the layer ARMED | **FIXED** | `ba0b0d7` — `SignatureHealth` has no `null`; unknown travels as a hierarchy row |
| F-2 | `^` anchors to byte 0, not line start — three patterns dead | **FIXED** | `5920c92` — folded into H-4 (same file, same class) |
| M-4 | A classifier that ran and failed is indistinguishable from clean | **FIXED** | `5920c92` — `Scored.failed` → `incomplete`, scored windows kept |
| N-8 | Unbounded `previous/classifier/<version>` retention | **FIXED** | `5920c92` — died with the classifier component |
| N-10 | Corpus-echo bundle passes the whole gauntlet | **PARTIAL** | `5920c92` — `coverage_floor` catches the curation case, not a hostile publisher |
| N-12 | 512 MiB aggregate-download exposure | **FIXED** | `5920c92` — died with the classifier component |
| O-1 | The H-1 race guard is `#[ignore]`d and CI never runs `--ignored` | **FIXED** | 2026-08-09 — `tests.yml` gains a `cargo test (ignored, node-backed)` step; see *Test & CI gaps* |
| S-1 | No dependency-advisory job in CI | **FIXED (detector only)** | 2026-08-09 — new `.github/workflows/advisories.yml`. The 16 wasmtime advisories are **still open and still undecided** (open question 4) |
| — | Pre-existing 1-in-4 test flake on the global rule slot | **FIXED** | `5920c92` — guard made structural in `screen_blocking` |
| F-3 | The primary contamination path writes no forensic record at all | **FIXED** | `ce7c54a` — `Screen::Contamination` on the transition, both paths, one helper |
| F-4 | `is_configured_tab` is agent-agnostic — `(consumer, tab)` verified nowhere | **OPEN** | raised by the fix run |
| F-5 | `/graph_run` + `/mcp/call` share H-8's tab half | **OPEN** | raised by the fix run; a decision, not a bug |
| F-6 | H-2's decode proof degrades silently if the CLI drops `sessionId` | **OPEN** | raised by the fix run; wants a drift canary |
| F-7 | Auto-injection still pushes signatures into a contaminated tab | **OPEN** | raised by the fix run; bounds what H-1 claims |
| F-8 | A denied URL still leaks its hostname to DNS | **OPEN** | raised by the fix run; bounds what "denied" means |
| F-9 | The signature scan's 1 s budget is wall clock spanning both passes | **OPEN — DECIDED 2026-08-11, ready to implement** | **Options 1 + 3: give the normalized pass its OWN budget instead of the remainder, and size both now that H-4 doubled the work.** Rationale + the two rejected options in the F-9 entry below. **SIZING CORRECTED 2026-08-11 — see F-9b: it must be 2 s PER PASS (`SCAN_PASS_TIMEOUT`), not 2 s total, or the fix is a no-op.** Floor becomes 1 s ≈ 4.8× the worst legal input (256 KiB ⇒ ~210 ms/pass, derived from the measured 105 ms per 64 KiB across both passes). **Worst-case wall clock 1 s → 4 s**; no caller bound needs changing (nearest are 30 s and 45 s). `SCAN_TIMEOUT` is to be **deleted, not aliased**, so its one consumer (`validate.rs:70`) becomes a compile error — a tripwire, per session 4's lesson. `DidNotComplete` must also name the pass (`raw`/`normalized`): today both passes emit identical strings and `merged_with`'s dedupe hides which layer died |
| F-9a | `Hits ⊕ DidNotComplete = Hits` — merging outcomes **drops the incompleteness**, so a truncated scan that happened to find something reports as a *complete* scan | **OPEN** | raised 2026-08-11 while designing F-9's fix, recorded rather than folded in as silent scope creep. Same family as M-5: the honest signal exists and is then discarded. Note the related fail-open is *narrowed* by F-9's fix but not closed — truncation cannot be mistaken for clean through `scan_outcome_with`, but it **can** through the hits-only `scan_with` used by `graph::secrets` and the updater gauntlet |
| F-9b | `yara-x`'s `set_timeout` is `ceil()`ed to whole seconds and compared against a free-running **process-wide 1 Hz heartbeat**, so a pass asked for N s aborts anywhere in `(N-1, N]` — **at N=1 the guaranteed floor is ZERO** | **OPEN — subsumed by F-9's fix** | found 2026-08-11 in the dependency source (`yara-x-1.12.0/src/scanner/context.rs:549-586`, `:762`). **This is F-9's real mechanism — not work volume — and it invalidates the ledger's own proposed sizing:** raising `SCAN_TIMEOUT` to "2 s total" is a **no-op**, because a shared budget still `ceil()`s each pass back to a zero floor. The fix must be 2 s **per pass**. Recorded separately because it is a property of the dependency that will outlive this fix and must be re-checked on every `yara-x` upgrade |
| F-18 | Every pointer to the V32 controls names a Settings section that does not exist | **OPEN** | found live 2026-08-10 on v0.51.0-rc.1 — the first defect found by *running* the build |
| F-19 | No `claude-opus-5` row in the seeded price table — session cost reads $0 | **FIXED** | `1524efa` — seed row **plus** a `pricing_seeded_generation` watermark migration for existing installs; tripwire `every_built_in_priced_model_is_reachable_by_existing_installs` |
| F-10 | `NativeRouter` never re-gated `graph_*`/audit tools — `LOCAL_DATA_TOOLS` lists only read_file/list_dir/code_search/run_command/filesystem/git | **FIXED** | `f895e74` — carried by M-2's `GatePass`; exploitable half was `graph_*` on a cloud/LAN backend via the headless child |
| F-11 | `select_discovery` prefers the DEEPEST matching root — one `Write` of a well-formed discovery file with a dead port forces the fallback *and* chooses the reason it reports | **OPEN** | steers `audit/mcp.rs` and every hook shim; decide with F-26, which shows the same file set is larger than documented |
| F-12 | `run_check` is **advertised** to a cloud/LAN backend and executes the project's configured commands | **OPEN — DECIDED 2026-08-11, ready to implement** | HIGH. **User decision: add `run_check` to `LOCAL_DATA_TOOLS`, denying it to cloud backends BY DEFAULT, with a user opt-in.** Both halves are required and neither is sufficient alone: `LOCAL_DATA_TOOLS` fixes **new** backends, and the `BackendGate` opt-in must be enforced **at call time** so **existing** configured backends are fixed too (this is exactly F-10's shape — *"the helper is right, the call site is missing"*). Implement with M-8's residual and M-7 residual #1, one family |
| F-13 | `/latch/state` publishes `contaminated`, but the OpenCode plugin's gate reads only `st.latch`, so a **contaminated-but-not-EXTERNAL** tab admits every local tool | **OPEN** | **reached live 2026-08-11** — an OpenCode tab sat `contaminated:true` with `latch:"open"` and rendered a contamination badge in that state. Signal with no consumer; decide with the contamination-clear path |
| F-14 | H-1's "most hardened combination" comment overstates what gate-ON + beacon-OFF invalidates | **OPEN** | LOW. Bounded live by recipe 19's second half, which verified the invalidation *does* sit above the beacon's enable guard |
| F-15 | `GraphIndex::mem_add_note` is still `pub` and takes a raw `&str`, so M-20's `NoteText` guards the *path*, not the *store* | **OPEN** | same shape as the `mem_quarantined_notes` tripwire gap; decide the two together |
| F-16 | Two of three `MemoryQuarantine` row producers write `root: ""` | **OPEN — RE-RATE** | filed LOW, but **reproduced live 2026-08-10**: of the two rows a dual-trip note writes, the one carrying `root:""` is the **secret-screen** row — in a root-filtered view the row recording that a *credential* was held is the one that vanishes |
| F-17 | Stale `cimp-<hash>.exe` from completed test runs wedges the next link with `LNK1104` | **OPEN** | LOW product / HIGH iteration cost; plausibly the same child-process leak as the two `audit::runner` kill-timing flakes — one fix may close both |
| F-20 | Every proxied call's own `kind:"mcp"` row is `tab:"unattributed"`, and `graph` rows are `tab:"headless"`, while the `injection_flag` rows beside them name the tab | **OPEN — NOW CHEAP** | **sharpened live 2026-08-11**: `refuse_headless`'s `ok:false` rows **do** name the tab while the served path's `ok:true` rows do not — same file, same `--tab`. The correct implementation is one function away (`graph/mcp.rs:744-747`); diff the two paths |
| F-21 | "The worker withholds defs, the proxy refuses the call" is wrong as a consumer-level rule — it is per **MODE** | **FIXED (docs)** 2026-08-11 | locked decision 2 amended in place (`MILESTONE-V32` §"Design — locked decisions" 2), live-verification items 1–2 re-worded per mode, checklist cross-ref added. Every replacement claim was re-verified against source: `toolclass.rs:551-556` (`Profile::latch`), `:220` (`graph_snippet` is LOCAL-CAPABILITY), `:582-592` (`Latch::blocks`, one predicate ⇒ containment identical in both modes), `:899-904` (`filter_defs` is an identity at `Latch::Open`), `agent.rs:1484-1495`/`1773-1776`/`1535-1544` (latch engages mid-turn; the advertised view is rebuilt only at the top of the NEXT step, so the turn that engages it was assembled unfiltered), `loopback.rs:10670-10677` (`/mcp/list` — the proxy enforces by refusal **by design**, because consumers cache `tools/list` at connect, so decision 3 was never wrong). Original text left intact under an `Amended 2026-08-11` block. Same-class doc errors swept: `ARCHITECTURE.md` + `DESIGN.md` both claimed ONE discovery file — corrected to both paths (F-26's class). **F-26's in-code comment is still wrong and is owed on the Rust lane.** doc accuracy. Under a declared `profile` the other side's defs are genuinely absent; with `profile` omitted the worker keeps the full list and is refused at the gate. Containment intact either way. Same class as O-1 and F-26 |
| F-22 | *(withdrawn)* live settings vs `settings.json` disagreement on `native_web_visibility` | **WITHDRAWN** | `13d3f2d`; issue [#52](https://github.com/Dyserna/cImp/issues/52) closed as invalid. My error — compared the resolved value against the global file alone, ignoring the sparse project overlay. **Read both config levels before any settings claim** |
| F-23 | After a **user** clicks "Switch to local", the refusal still says *"this task has already used a local-capability tool"* — a cause that did not happen | **OPEN** | the latch already writes a `latch_override` row with `origin:"ipc"`; the refusal just does not consult it. The quarantine notice next door DOES state a cause it checked — copy that pattern |
| F-24 | The Memory view's *Quarantined notes* card shows text + time + Promote/Discard only — **no screen, no rule, no reason** | **OPEN** | MEDIUM. Shows the **secret value** and withholds the **rule name** — the inversion of decision 22. The reason exists and reaches the model, which cannot act; not the human, who must. Take with M-23 + M-24 |
| F-25 | The detection updater's scheduler did not tick for 34+ min with due-ness forced | **DEFERRED** | LEAD. Split to issue [#53](https://github.com/Dyserna/cImp/issues/53) with all updater work. Settle with one `RUST_LOG=offload=info` run; hypothesis is the spawned task dies silently |
| F-26 | `graph/mcp.rs:609-613`'s one-byte repro does not reproduce — there are **two** discovery paths and the legacy one still resolves | **OPEN** | raised live 2026-08-11. Cuts both ways: attack surface is *narrower* than documented (nothing about M-2 is weakened), but following the doc literally yields a **false PASS**. Fix the comment to name both files and say which `select_discovery` prefers |

Everything in the MEDIUM table below is **OPEN** unless its row says otherwise.

**Not a finding, but the reason several of these survived:** three regression
tests pin the defect's *shape* rather than the invariant (see *The pattern*).
Any fix run here should check what the existing test would still pass with.

---

## The pattern

Ten of the findings below share one shape, and it is worth stating before the list
because it predicts where the next one will be.

**The fixes are correct against their proof-of-concept and incomplete against their
invariant.** In three cases the regression test pins the PoC's shape, so the gap is
invisible to the suite:

- C-2's test asserts `gate.observed(0, 0)` — the *zero-byte* file from the original
  PoC. Any newline-terminated byte defeats the fix. (`loopback.rs:6504`)
- The spotlighting spawn-baked test explicitly lists `"spotlighting L3"` under
  `stays` and asserts it *must not* move the spawn signature — pinning the defect
  as correct. (`injection.rs:1736`)
- `withSignatureHealth(base, null)).toBe(base)` pins "detection status unavailable
  renders as protected". (`latch.test.ts:227`)

Two further cases fix the reported strings rather than the rule: C-4 was fixed in
three separate commits and the general case is still open (H-3); the `#46` outcome
split was fixed forward-only once, then again, and still misses the artifact-fetch
call site (M-9).

A second pattern, narrower but sharper: **two individually-correct fixes that are
jointly fatal.** H-5 exists only because the U-1 hardening (ban redirects) met a
channel URL that always redirects. No single-module reviewer could see it.

---

## HIGH findings

### H-1 — C-1 is OPEN: the TRUSTED class carries a general source-text read primitive

> **STATUS: FIXED — `0169d10`.** User-decided shape: **demote both + strip signatures**, recorded as **locked decision 29** and the **Phase A amendment 2026-08-08 (c)** in the milestone — a defect in the locked decision itself, not code drift from it, so the taxonomy table was corrected rather than silently edited.
>
> `graph_struct_search` and `graph_repo_map` are `ToolClass::LocalCapability`. `fmt_symbols` — the shared renderer behind `graph_find_symbol` / `graph_outline` / `graph_callers` / `graph_callees` — no longer emits `signature` at all; those four keep name, kind, path, line, the `[test]` tag and the V15 confidence badge. The strip is **unconditional**, not latch-conditional: the four callers are always TRUSTED, so the condition would always be true and would be one more thing a future caller can get wrong.
>
> **The seam is at the model-facing MCP output only.** The index still stores `signature`, `SymbolHit` still carries it, and every human-facing consumer still renders it — the `graph_dead_exports` Tauri command behind Code Intelligence, `context::read_advice`'s outline line, `context::file_digest` and `context::repo_map` in auto-injection. A human reading their own repo is not the threat model; a blanket removal would have been a regression dressed as a fix, so a test asserts two of those consumers still get it.
>
> **Both enforcement paths verified by reading the call sites, not by trusting the table** — the way C-1 survived `b80f5b8`. Worker: `filter_defs` (`agent.rs:1507,1554`) + `latch_gate` (`agent.rs:1259`). Proxy/tab: `LatchRegistry::gate` (`loopback.rs:1630`) on `/graph_run` (`loopback.rs:2377`), which is the **only** path for a tab — the proxy never def-filters the graph surface. Both resolve through `classify()`, so no new route was needed. Tests assert each name on each path.
>
> `graph_repo_map`'s 200,000-char `budget_chars` clamp (`graph/mcp.rs::run_repo_map`) is unchanged and still applies; the tool is now latch-gated *in addition to* being clamped. No internal caller breaks: session-start repo-map auto-injection calls `context::repo_map` in-process (`graph/service.rs:2480`), never the tool.
>
> **Deferred, not implemented:** literal redaction inside the signature (keep the type and name, blank the value) — better information-per-risk, but per-language tree-sitter work whose failure mode is a silent leak. Its own decision, per decision 29.

> **STATUS: OPEN.** Two reviewers found this independently from opposite entry points; verified by hand. The locked taxonomy table lists `graph_struct_search` under TRUSTED too, so closing it needs a **decision amendment**, not just a `TABLE` edit.

`offload/toolclass.rs:205,211` · `graph/mcp.rs:1929-1944` · `graph/builder.rs:838-845`

Independently found by two reviewers from opposite entry points; verified by hand.

`graph_struct_search` is `ToolClass::Trusted`. Its executor reads files **off disk at
call time** across every indexed file of a language, runs a caller-supplied
tree-sitter query, and returns matched text — 100 rows × 2000 chars with shipped
defaults. It is `code_search` with an AST filter. `Latch::blocks` never blocks
TRUSTED and `filter_defs` never strips it.

`signature_of` is `first_line(node_text(..))` capped at 200 chars — **the
definition's first source line**, not a name. Rust `const_item`/`static_item` are
indexed symbols, so `graph_find_symbol{name:"STRIPE_SECRET"}` returns
`const STRIPE_SECRET: &str = "sk_live_…";` verbatim. That backs `graph_outline`,
`graph_find_symbol`, `graph_callers`, `graph_callees`. `graph_repo_map` packs
signatures to a model-supplied `budget_chars` clamped to 200,000.

**Exploit.** Under an EXTERNAL latch, `graph_snippet`/`read_file`/`code_search` are
stripped from the advertised tool list while `graph_struct_search` stays. An
injected page requests `(string_literal) @s`, gets the repo's literals, and
exfiltrates through the still-live `ddg__search`. Full trifecta, through the class
declared clean.

**Why it survived:** the fix run demoted `run_check`, `security_audit`,
`quality_audit`, `offload_task`, `offload_batch` and never touched the structural
graph tools — *including `graph_repo_map`, which C-1's own failure scenario names by
hand*. The class doc (`toolclass.rs:52-58`) still asserts these "return
names/edges/metadata, and that is the whole of the class's clean case."

**Note on fix shape:** the spec's own locked table (`MILESTONE-V32:54`) lists
`graph_struct_search` under TRUSTED. This is a defect in the locked decision, not
code/spec drift — closing it needs a decision amendment, not just a `TABLE` edit.

**Fix.** `row("graph_struct_search", LocalCapability)` and `row("graph_repo_map",
LocalCapability)`; bump the TRUSTED-count assertion (`toolclass.rs:772`) 18→16;
amend the locked table. For the `fmt_symbols` four, either demote or strip the
`signature` field when the call is TRUSTED-classed — the latter keeps their
navigational value, which is why the class exists.

### H-2 — C-2 is OPEN: contamination clears on any newline-terminated byte

> **STATUS: FIXED — `2c40136`.** Fixed by *removing* the reset, not by raising the bar: no filesystem-derived fact can be the trust root for un-tainting a context window, because decision 3 puts the model's shell outside every latch and the model therefore writes the very file the rotation is inferred from. C-2 raised "the file exists" to "the file grew"; H-2 walked over that with one newline; "it decodes" would fall to `echo '{"sessionId":"…"}'`. `TabLatch::observe` still resets latch/budget/`latch_flagged`/`beacon_flagged` — permissive state the next real call re-earns — but not `contaminated`. **Amends locked decision 15: contamination is a property of the tab, not the conversation**, because the conversation boundary is attacker-controlled.
>
> Defence in depth: `LiveSessionGate::observed` now takes a decode proof instead of an offset delta — `drain_new_lines` returns `Drained { offset, own_record }`, with `own_record` set only inside the parse arm for a line whose top-level `sessionId` matches, so an unparseable line can neither be evidence nor abort the drain, and the offset advance stays unconditional and independent.
>
> The three tests that pinned the old rule were **inverted, not deleted** — including the C-2 regression that asserted `gate.observed(0, 0)`, the zero-byte PoC shape this finding walked over.
>
> **Consequence, deliberate and user-accepted:** the registry has no eviction, so `contaminated` now clears only on an app restart. Six user-facing strings that said "restarting the tab is the only clean reset" became false and were rewritten. A user-driven clear path (Timeline evidence + an explicit clear action) is designed and scheduled as a V32 phase before release — see the new findings section.

`oob/claude.rs:271-277,1544-1559` · `offload/loopback.rs:1280-1292`

Found independently by three reviewers; verified by hand across all three links.

`observe()` on a changed session id sets `latch = Open`, `budget.reset()`,
`contaminated = false` — the comment calls it *"the ONE place contamination is
cleared."* `LiveSessionGate::observed(before, after)` confirms on `after != before`.
`read_complete_lines` advances the offset for **any newline-terminated bytes** — no
JSON parse, no session-id check. A trailing `\n` is the entire bar.

The fix rejects `type nul >` (zero bytes) and accepts `echo {} > <newest>.jsonl`.
Claude's native Bash is outside every cImp latch by decision 3, so this is one
command. The consequence is the memory-persistence channel decision 10 exists to
close — and `/latch/override` deliberately *cannot* do this.

The module doc states the rule as *"Growth is the proof, and it is the only proof."*
Growth proves something is writing, not that the harness is writing.

**Fix.** Require the confirming bytes to decode as a transcript record with a
matching `sessionId` (the drain already parses each line — thread "at least one
recognised entry decoded" out instead of the raw offset delta); or decouple
`contaminated = false` from rotation and clear it only on tab teardown.

### H-3 — SSRF: the screen matches literal `http://`, the fetcher accepts `http:`, `http:/`, `http:\`

> **STATUS: FIXED — `e5f3627`.** `URL_PREFIXES` → `URL_SCHEMES` (`["http:", "https:"]`): the slashes left the constant and are **parsed, not matched**. `scan_scheme_runs` matches the scheme case-insensitively, consumes the slash run (zero length allowed), and emits a candidate normalized to `//`. `\` is dropped from the terminator set only *inside a scheme-bearing run* — it stays a terminator for `scan_bare_authorities`, where it is a Windows path separator.
>
> **The test is the fix.** This was the fourth attempt at this class (C-4 was closed in three separate commits, each fixing the reported strings rather than the rule), so the 29-literal table was demoted to a supplementary pin and replaced as the guard by a **generated corpus**: scheme × slash-run × infix × authority × tail = **15,840 cases**, oracle `Url::parse` from the pinned `url 2.5.8`, applied both as-written and as WHATWG strips it, asserted both bare and buried in prose. Its bound is a parallel **5,280-case public control** that must all pass, so an extractor that denies everything fails too. The infix axis is *every character the extractor treats as a terminator*, so the corpus **audits** the `is_scheme_only` exemption rather than restating it. Verified RED against the pre-fix extractor: **1170 of 5760 oracle hits evaded; 0 after.**
>
> **`is_scheme_only` KEPT, argument narrowed.** Row 5 (`http://\10.0.0.1`) turned out to be an *extraction* defect wearing the exemption's clothes — after the fix that string extracts and is denied, and the exemption never sees it. The old doc was wrong twice, not once: `\` is a slash for special schemes, **and** quotes are not forbidden host code points either. The replacement reasoning is per-terminator and machine-checked by the corpus rather than asserted.
>
> **Bonus, pre-existing false *refusal* found by the public control:** the flat trailing-punctuation trim ate the closing bracket of any bracketed-IPv6 URL ending at its authority, so `http://[2001:db8::1]` became unparseable and the deny-on-unparseable rule refused a *public* target. The trim is now balance-aware. Same screen/parser disagreement as H-3, pointing the other way.

`offload/outbound.rs:239,343-364,738-749`

Verified by the reviewer compiling the extractor against the app's own `url 2.5.8`
and executing it; premise confirmed by hand.

`URL_PREFIXES` is `["http://", "https://"]` matched as a literal substring. WHATWG
special-scheme parsing consumes *any number* of `/` or `\` after `http:`, including
zero. `scan_bare_authorities` cannot rescue these (its `authority` becomes the
string `"http:"`, rejected as implausible). Executed results:

| Argument | Extracted | `Url::parse` sees |
|---|---|---|
| `http:/127.0.0.1:12344/props` | `[]` → **allowed** | host `127.0.0.1`, port `12344` |
| `http:127.0.0.1:12344/props` | `[]` → allowed | host `127.0.0.1`, port `12344` |
| `http:/169.254.169.254/latest/meta-data/…` | `[]` → allowed | link-local, full path |
| `http:\\127.0.0.1\props` | `[]` → allowed | host `127.0.0.1` |
| `http://\10.0.0.1` | `["http://"]` → scheme-only exempt | host `10.0.0.1` |

Row 1 is the exact target C-4 named — the local `llama-server`, the one internal
service decision 11's carve-out text calls deliberately unprotected — reached with
bare ASCII containing no control characters.

**This disproves the code's own safety argument.** `is_scheme_only`'s doc
(`outbound.rs:744-746`) justifies exempting a bare `http://` by asserting every
other terminator is "a forbidden host code point and no parser will fetch past it",
listing `\` among them. For special schemes `\` **is** a slash. The same false
sentence is copied into the spec at `MILESTONE-V32:436-438`.

**Fix.** Match the scheme case-insensitively as `https?:` followed by zero or more
`/` or `\`, normalized to `//`; stop treating `\` as a run terminator inside a
scheme-bearing run. Then replace the 29-literal table test with the invariant: *for
every argument string, as written and stripped, if `Url::parse` yields a host that
`is_denied_ip` rejects, `screen_urls` must deny.*

### H-4 — every shipped detection rule is defeated by whitespace the reader cannot see

> **STATUS: FIXED — `5920c92`.** Rules widened to `\s{1,8}`, plus `signature::normalize_for_scan` and a second unioned scan pass, plus three smoke-corpus controls so the updater’s gauntlet enforces it on every future bundle. Measured 20/50 hostile probes evading to **0/53**, with 15 benign controls still clean. Locked decision 7 amended.

`detection/rules.d/*.yar` — all nine text rules

Empirically verified: the reviewer compiled the shipped rules with the pinned
`yara-x 1.12.0` and scanned probe documents. Every gap is `[ \t]{1,4}` or
`[^\n]{0,N}`, and yara-x is built `.unicode(false)`, so these are byte classes with
no Unicode-whitespace semantics.

```
"Ignore all previous instructions."          → HIT
"Ignore all previous\ninstructions."         → MISS
"Ignore\u{a0}all\u{a0}previous\u{a0}…"       → MISS   (NBSP)
"Ignore     all previous instructions."      → MISS   (5 spaces)
"Ig\u{200b}nore all previous instructions."  → MISS   (one ZWSP)
```

Same across every family — `RoleReassignment`, `CovertChannel`, `Exfil_SendToUrl`,
`Exfil_FetchParam`, `ToolSteering_Secret`, `HtmlCommentImperative`: plain HITs,
wrapped MISSes. `CImp_Obfuscation_ZeroWidthRun` does not backstop it — it requires
`#zw > 24` (verified: 24 MISS, 25 HIT) and evading a keyword costs one.

**Severity driver:** the classifier is inert on every install today (Prompt Guard 2
weights unpublished), so the signature layer *is* the detection surface. A page with
`&#160;` between words renders identically in every browser and produces no warning
header, no activity row, no log line. The line-wrap half additionally fires on
non-adversarial pages: any HTML→text extractor wrapping at 80 columns breaks a
payload for free.

Tests miss it because every hostile string in the unit tests, in `HOSTILE`, and in
`detection/smoke/hostile/` is single-line single-spaced ASCII.

**Fix (validated by the reviewer, benign controls preserved).** Replace inter-token
`[ \t]{1,4}` with `(\s|\xc2\xa0|\xe2\x80\x8b|\xe2\x80\x8c|\xe2\x80\x8d){1,12}`
(byte escapes — yara-x rejects `\x{…}` under `.unicode(false)`). Add wrapped/NBSP/
ZWSP variants to `detection/smoke/hostile/` so the updater's gauntlet enforces it on
every future bundle.

### H-5 — the detection update channel is inert by construction

> **STATUS: FIXED — `5920c92`.** Channel repointed to a `raw.githubusercontent.com` branch ref (verified 200, no redirect), and the missing guard added: the pinned URL is now asserted against our own redirect policy, with release-asset paths rejected by name. Locked decision 27; decision 24 unblocked.

`updater/manifest.rs:133` (pinned URL) vs `manifest.rs:692-697`
(`redirect::Policy::none()`)

Verified live: GitHub release-asset paths answer `HTTP/1.1 302 Found` →
`release-assets.githubusercontent.com`. The pinned `DEFAULT_MANIFEST_URL` is a
release-asset path. Redirects are banned — deliberately, as U-1 hardening.

So on the day `detection-v1` is published, every install fetches, gets a 302, and
classifies it `Outcome::Unavailable` — *deliberately silent, no card*. Rules never
update on any install. After 7 checks a stall card appears reading *"HTTP 302
Found"*. The first week of the failure is invisible by design.

The redirect ban would also break if followed naively: the CDN host differs and
carries a query, so those URLs cannot be named in a manifest either
(`AssetAnchor::accepts` rejects both). Nobody noticed because `detection-v1` is
still 404 — it was never published.

**Fix.** Either allow a bounded re-validated hop (`Policy::custom`, ≤2 hops, https
required, host pinned to `objects.githubusercontent.com` /
`release-assets.githubusercontent.com`), or move the channel to a host that answers
200 (`raw.githubusercontent.com/…`). Add a test that the pinned URL's host is one
the fetch policy can complete against — today nothing connects the two.

### H-6 — DECLINED 2026-08-08 (user decision). Not fixing; reasoning recorded.

**Decision: cImp will not sign the detection bundle.** Recorded here rather than
silently dropped, per global principle 10 (every HIGH gets an explicit
close-or-defer with an owner).

The finding is factually correct — the bundle is authenticated by TLS plus
`contents: write` on the repo, and the manifest carries its own hashes. What the
finding got wrong is the *value* of the proposed fix.

**A signature only raises the bar if the key is somewhere the compromise cannot
reach.** The channel's trust root is `contents: write` on `Dyserna/cImp` — the
same repo, and the same `GITHUB_TOKEN`, that publishes the cImp **binary**
(`release.yml:13-14`, workflow-level, with `gh release` running in the same jobs
as `cargo build` and `npm ci`). Anyone who can publish a detection bundle can
publish a cImp release with detection removed outright, which is strictly worse
and strictly easier. A signing key usable by that release process would live in
the same GitHub org/CI, i.e. inside the blast radius it is meant to bound. For a
solo-maintainer project where the same credentials publish both artifacts, that
is ceremony, not security.

Signing becomes worth revisiting only if the key moves outside CI — an offline
maintainer key, signing locally before upload. That is a real option and a real
cost; it is not what this finding proposed.

**Two things this decision leaves open, deliberately:**

1. **`release.yml`'s workflow-level `contents: write`** is still broader than it
   needs to be — every build step in both jobs holds a token that can rewrite
   any release. Narrowing it means splitting build from publish (artifacts +
   a separate `publish` job with the write scope). That is worth doing on its
   own merits and is **not** detection-specific — it protects the binary release
   too, which is the larger prize. Recorded as a maintenance item, not a V32
   blocker.
2. **N-10 (corpus echo)** was the residual that actually carried risk here, and
   it is now **partly closed** — see below.

**N-10 — coverage floor added** (`updater/mod.rs::coverage_floor`). A candidate
bundle whose rule count is less than half the live shipped set is refused, with
the live baseline read from `store::managed_rule_files` so a user's `local/`
rules can never inflate it. Framed honestly in its own doc comment as a
**curation guard, not an anti-tamper control**: a hostile publisher controls the
count too. What it does catch is the far likelier failure — a maintainer
publishing a half-built bundle, which today would pass the entire gauntlet,
because the positive control is the shipped `smoke/hostile/` corpus and a bundle
matching only those documents satisfies every other gate.

---

### H-6 (original finding) — the detection bundle is authenticated by TLS and repo-write access only

`updater/manifest.rs:133` · `mod.rs:1060-1076` · `.github/workflows/release.yml:13-14`

The manifest carries its own SHA-256 digests, fetched over the same channel with the
same trust. There is no signature, no detached digest in the binary, no key pinning
beyond TLS. Artifact verification protects against corruption and against an
attacker who controls only the blobs — not against anyone who can write the
manifest.

And `permissions: contents: write` is declared at **workflow** level, so the
`GITHUB_TOKEN` in every job — including those running `cargo build`, `build.rs` and
`npm ci` over the full dependency tree — can create or clobber assets on the
`detection-v1` release. A compromised transitive dependency reaches the security
layer's own update channel. There is no job that builds, hashes or publishes the
bundle; it is entirely manual.

Decision 13's stated purpose was to move the trust boundary off Vigil/garak. It
moved it to a boundary with no cryptographic control.

**Fix.** Embed a public key and require a detached signature over the manifest bytes,
verified *before* `Manifest::parse`; failure is `Rejected` and cards. Much cheaper
second-best: narrow `permissions:` to the publishing job and add
`actions/attest-build-provenance` for the detection assets.

### H-7 — a cloned repo's `opencode.json` is executed configuration

> **STATUS: OPEN.** Verified live against the installed OpenCode via `opencode debug config` and `debug agent build`. Mostly V33 territory (OS-level containment), but worth recording as *known-open* rather than left implicit — the docs currently present the additive posture as benign.

`tabs/config.rs:1954,1350,1435,1887`

Verified live against the installed OpenCode via `opencode debug config` /
`opencode debug agent build`. cImp deliberately runs OpenCode additively (it does
not set `OPENCODE_DISABLE_PROJECT_CONFIG`).

- Keys the pin **writes** (`bash`/`edit`/`webfetch`/`websearch`) — **cImp wins.**
  Decision 8's pin is genuinely real.
- Keys the pin does **not** write merge in and take effect:
  - `mcp: {"evil": {"command": [...]}}` → arbitrary local command spawned at launch
  - `plugin: ["file:///…"]` → registered, `scope: "local"`
  - `instructions: ["EVIL.md"]` → resolves **before** cImp's Phase D contract
    paragraph, defeating the "contract goes FIRST" ordering at `config.rs:934`

Chains badly: OpenCode `write` is pinned `allow`, and `git_exclude_opencode` adds
`.opencode/` to `.git/info/exclude`, so a plugin file the agent itself writes never
appears in `git status` and loads on the next launch. `sweep_stale_opencode_plugins`
explicitly leaves non-`cimp-inject-*` files alone. A hostile plugin replacing
`globalThis.fetch` would neuter the Phase F beacon and Phase H gate, which call the
global lazily inside their hooks — while `/status` and the Settings badge report
both ON. *(The monkeypatch step is inference, not executed — see Open questions.)*

**Fix.** (1) Resolve `fetch` into a private binding at plugin module load. (2) Ship
the `OPENCODE_DISABLE_PROJECT_CONFIG` setting V19 §A.3 already scoped, default on
for tabs with the Phase H gate enabled. (3) At minimum correct
`build_opencode_config`'s doc, which presents the additive posture as benign and
never names what else merges in.

### H-8 — `POST /audit/run`'s taint gate is opt-in by the caller

> **STATUS: FIXED — `80375a9`.** Both halves closed, not the "or" below. `tab` is now required (refused post-parse, not via serde: the child turns any non-200 into `"cImp returned 400: …"`, a truncated protocol complaint instead of the actionable "restart this tab in cImp — its MCP child is from an older build"; post-parse also trims `""`/`"   "`, the exact shapes a caller would use to opt back out, which serde cannot). `consumer` is narrowed at the parse boundary to `{claude, opencode}`.
>
> **The `"offload"` arm was worse than described.** Both spawn sites were traced: `tabs/config.rs:886` sends no `--consumer` (the child defaults `claude`), `:2117` sends `opencode`. The offload worker calls `run_audit` **in-process and never opens a socket** — `settings/schema.rs:1390-1398` says so outright ("`expose_offload` is deliberately absent: the offload worker runs in-process"). So `consumer_exposed`'s `"offload" => expose_offload` arm was reachable over HTTP *only* by a caller no legitimate component ever is, and it defaults **true** while `expose_claude` additionally requires the master `enabled`. `audit_consumer` returns `&'static str`, so the caller's own string never reaches `consumer_exposed` at all.
>
> The decision is extracted to `audit_admit` — ordered consumer → expose → cwd → tab → gate, dependencies injected as closures. That makes "a refusal engages no latch" testable as an *observation* rather than a tautology, routes all four refusals through one arm before `write_ndjson_head` (preserving the single-JSON-over-200 shape `parse_result_line` relies on), and makes `gate()` unreachable without every prior check passing. The `warn!` is now written over `scope.is_none()`, covering `Anonymous` and any future variant rather than `Unknown` alone.
>
> **Known ceiling:** the unknown-tab test cannot assert the `warn!` itself (not observable without a tracing-capture harness); it pins the *predicate* so a regression is visible in review.
>
> **Correction to this report:** the fail-open cited below as `loopback.rs:1583-1585` is really **`1626-1629`** — 1583-1585 is the `live_settings` call.

`offload/loopback.rs:2553-2587` · fail-open at `loopback.rs:1583-1585`

Verified by hand. The gate's only identity input is `body.tab`, caller-supplied.
Absent ⇒ `LatchScoping::Anonymous` ⇒ `scope() == None` ⇒ `gate()` returns
`Ok(WriteTaint::Clean)` before classifying anything. Only the `Unknown` arm (a tab
id naming no configured tab) warns; the `Anonymous` arm does not even log.

Compounding: `consumer` is caller-asserted, unvalidated and unbounded, and selects
which expose toggle is checked — including `"offload"`, which defaults **true** and
which no legitimate caller sends. `ada4bae` validated `/run`'s `tool` label at the
parse boundary in the same commit and left `consumer` alone.

**Exploit.** An EXTERNAL-latched tab curls `/audit/run` with the discovery-file
bearer token and no `tab`. It receives the full gitleaks report; its latch is
neither consulted nor engaged; `ddg__search` stays open to carry the secrets out.
Decision 3's residual covers *capability* ("a model with a shell already has this");
what leaks here is *latch state*.

**Fix.** Make `tab` required on `/audit/run` (both spawn paths already send it), or
restrict `consumer` to `{"claude","opencode"}` at the parse boundary.

### H-9 — forensic activity rows are still model-evictable

> **STATUS: FIXED — but NOT by the fix proposed below, which does not close it.**
> A `pinned: bool` from `Screen::is_forensic()` fails on its own exploit:
> `MemoryQuarantine` is simultaneously *in* the forensic set and *is* the flood
> vector, so 200 quarantine rows evict the `Canary`/`LatchBeacon` rows again —
> now inside the protected lane — and an unbounded pinned lane would make one
> row per `context_note` an unbounded growth channel on a file-backed store.
>
> What landed instead is a **retention lane per `Screen`** (`activity::Lane`,
> `enforce_kind_caps` → `enforce_lane_caps`): a lane is a kind, except for
> `injection_flag`, where it is one screen. Each lane keeps its own newest
> `INJECTION_FLAG_SCREEN_CAP` (64) rows and evicts only its own oldest, so
> **no screen's volume can cost another screen its history**, every screen stays
> bounded, and a chatty screen still keeps its own recent window. The lane set
> comes from `Screen::ALL`, emitted by a new `declare_screens!` macro from the
> variant list — so F-3's contamination-event row inherits the guarantee by
> existing, and sharing a lane would take a deliberate edit. Unrecognized
> sources (a newer file, a retired wire value) share one bounded catch-all lane.
>
> `FILE_COMPACT_LINES` is now derived (`TOTAL_CAPACITY + 500`, held by a
> `const _: () = assert!(…)`): the old literal `1000` was exactly the then-total
> capacity, i.e. a saturated store would re-read and rewrite the whole JSONL on
> every record, and adding lanes would have walked the write path onto that
> cliff silently.
>
> The guard test was fixed, not deleted, and eight tests were added: the literal
> exploit asserted **by content**, the reverse flood, the full
> flooder × victim matrix generated from `Screen::ALL`, boundedness under a
> flood of every screen, non-starvation, the catch-all lane, and survival across
> a compaction + reload round-trip. All eight go RED against the pre-fix
> eviction.
>
> Side finding closed on the way: `screen_labels_are_the_distinct_wire_values`
> iterated a hand-written ten-element array and had never seen
> `Screen::Unscreened` — the same drift `declare_origins!` was introduced to
> prevent, one enum over. It now iterates `Screen::ALL`.

`activity.rs:488-507,74` · `graph/mcp.rs:1146-1152,911-927`

`INJECTION_FLAG_CAP` is 200 across all eleven screens, evicted strictly
oldest-of-kind with no priority, pin, or screen-awareness. #48 bounded the SSRF row
with a doubling ledger and left `MemoryQuarantine` unbounded: the secret screen runs
on note content alone — no latch, no budget, no claim bit — and writes one row **per
note**.

**Exploit.** A canary exfiltration is caught (`Canary` + `LatchBeacon` rows). The
model then issues 200 `context_note` calls containing an `AKIA…`-shaped literal.
`context_note` has no fetch budget, no SSRF screen, no latch refusal. Two hundred
`MemoryQuarantine` rows evict the only forensic record of the attack that got in.
`Signature`/`Classifier` rows are a second unbounded source.

**The guard test asserts the breach.** `injection_flag_entries_keep_their_own_window`
(`activity.rs:825`) writes a row targeted `"a denial"`, floods `CAP + 10` more, and
asserts only that the count equals the cap. The first row must have been evicted and
the test never looks.

**Fix.** Add `pinned: bool` set from a `Screen::is_forensic()`
(`Canary | LatchBeacon | LatchOverride | MemoryQuarantine`); skip pinned rows in
`enforce_kind_caps` while an unpinned row of that kind remains, under a sub-cap. Add
`assert!(snap.iter().any(|e| e.target == "a denial"))` to the test.

### H-10 — a failed detection-status read renders the signature layer as ARMED

> **STATUS: FIXED — `ba0b0d7`.** **The type made the bug expressible**, so the type was the fix: `{armed} | null` conflated *a failed read* with *nothing to add*, and `SignatureHealth` is now `{armed} | 'unknown' | 'pending'` with **no `null` member** — the pass-through case must be typed as the word `'pending'`.
>
> **Both halves of the "or" below were needed**, and the second alone was insufficient. Letting the failure reach the outer `catch` would (a) discard the good hierarchy the backend just returned, (b) leave the tab badge and popover still rendering unknown-as-armed, since `injectionStatusUnknown` has no per-tab surface, and (c) make the chip claim it cannot read the switches when it can. So the read is caught **locally inside the same try**, and the uncertainty travels as a **row in the hierarchy** — where per-scope facts already live — while the health accounting reuses `recordPoll`, `UNKNOWN_AFTER_FAILURES = 3` and the existing 4 s tick with its own counter, because the two reads fail independently. `injectionStatusUnknown` keeps its exact meaning (*the hierarchy itself* is unreadable) and is untouched — setting it here would be the opposite lie, claiming total blindness while every switch is visible.
>
> Three states are distinguishable at **every** render site, not just in the model: the status chip gains a fourth word `unverified` (reusing `reduced` would point the user at a switch to flip, which is wrong when nothing was switched), and the tab badge, taint popover and Settings panel each render unknown distinctly. The popover no longer says "off" of a row it could not read. A mixed case states both claims in separate sentences rather than letting one outrank the other.
>
> `latch.test.ts:227` is **inverted, not deleted**, and paired with a test that unknown stays distinguishable from **not-armed** — collapsing those two is a *different* lie that the inverted test alone would pass. Verified RED against the pre-fix guard: 4 failed / 33 passed.
>
> **Known gap, not introduced here:** `vitest` runs in node with no DOM and the repo has no component harness, so render decisions were extracted to pure exported functions (the convention `latch.ts` already documents) and the tests assert the exact strings and class-driving values rather than component output. A component-level harness is a separate decision.
>
> **Rust side checked and clean** (report only): `detection_status` errors only on a `spawn_blocking` JoinError, `detection::status()` is infallible, and `armed = files_loaded > 0 && rules > 0` is the backend's own predicate — no path reports a failed read as armed.

`src/lib/latch.ts:255-258` · `offload.ts:300-307` · test `latch.test.ts:227`

Verified by hand.

```ts
if (!status || !rules || rules.armed) return status;
```

`rules === null` — what `detectionStatus()` returns when the IPC call throws, which
it swallows — takes the same branch as `rules.armed === true`. So "armed", "we
cannot tell", and "detection IPC is broken" all render as **fully protected**. The
call sits inside the `injection_status` try-block and cannot throw, so
`recordPoll(health, true)` still marks the tick healthy and `injectionStatusUnknown`
never trips.

This is D-2 reincarnated one layer up: D-2 was *"a failed reload silently disarms the
signature layer"*; this is *"a failed status read silently renders it armed."* And
`latch.test.ts:227` pins it as correct.

**Fix.** Make unknown a third state: `rules: {armed} | null | 'unknown'`, or invoke
`detection_status` non-swallowing inside the same try so a failure reaches
`recordPoll(health, false)`. Then invert the test.

---

## MEDIUM findings

| # | Finding | Status | Location |
|---|---|---|---|
| M-1 | Worker budget/latch is per-**attempt**, not per-task — `run_on` is called up to 4× (fail-over, thinking retry, tier escalation), each with a fresh `Budget::default()`. The 4 MiB/40-call cap is really 8 MiB/80 on a default install, 16 MiB/160 with fail-over configured. A loop that resets its own cap is not stopped. | OPEN | `agent.rs:1502-1505`, `service.rs:1147-1252` |
| M-2 | The A-1 fix **inverted a fail-closed default**: a native name missing from `TABLE` used to classify EXTERNAL and be refused/latched; now it "neither latches nor is refused" and falls through to dispatch. Load-bearing in both directions, with no test and no tripwire. | **FIXED (invariant made mechanical, both directions; the A-1 fix kept)** `f895e74` — see the M-2 note below; carries F-10's fix | `toolclass.rs` (`ClassRow::dispatchable`, `dispatchable`), `loopback.rs` (`LatchRoute::can_execute`, `LatchRegistry::gate`), `agent.rs::latch_gate`, `backend_gate.rs` |
| M-3 | Spotlighting is spawn-baked in practice (`fact_promotion_block` → `--append-system-prompt` / OpenCode instructions) but declared live: absent from `spawn_inject_sig`, no restart hint. Toggling it on mid-session leaves the running tab injecting **unenveloped pre-V32 memory into the system prompt**. Test pins the defect. | OPEN | `config.rs:1082-1089`, `injection.rs:330-337,1736` |
| M-4 | A classifier that runs and fails is indistinguishable from Clean, and a failing window **discards windows that already scored over threshold**. `Scored` has no field to express it. The spec names exactly two exclusions; the code implements three. | **FIXED** `5920c92` | `classifier.rs:391-399`, `mod.rs:404-409` |
| M-5 | At the worker, the "unscreened" notice is **false every time it fires**: `cap_result` truncates to 32 KB, unconditionally below both screening caps, so every byte the model sees was scanned. Trains the reader to discount a notice that is true at the proxy. | OPEN | `mod.rs:387-394`, `agent.rs:1910/1928` |
| M-6 | Audit findings enter model context with no envelope, no detection scan, and without contaminating the conversation — scanner-quoted text from `node_modules` is framed as authoritative project data, and `context_note` afterwards is *not* quarantined. `context_recall`, strictly tamer, *is* enveloped. | **FIXED (envelope + detection); contamination DECLINED with reasons** `dbfc027` — see the M-6 note below | `audit/mcp.rs:327-428,444-501`, `spotlight.rs:63-91,172-185`, `detection/mod.rs:555-584` |
| M-7 | Three ungated loopback routes reach local capability; `POST /context/post_edit` **executes the project's configured checks** for a caller-supplied `cwd`. None carries a `tab`; none appears in any route enumeration. | **FIXED (taint gate + enumeration)** `526c91f` — see the M-7 note below; the caller-supplied `cwd` half is **NOT** closed and is folded into H-7 | `loopback.rs:4095,4181,4690`, `toolclass.rs:234-266`, `config.rs:659,683,762,1872` |
| M-8 | `run_check` dispatches **above** the class gate on the headless path, so an EXTERNAL-latched tab that corrupts `.cimp-discovery` runs the project's build/test/lint while `ddg__*` stays live. | **FIXED (gate raised + widened to the class, keyed on the child's tab identity)** `4555d70` — see the M-8 note below; the residual is the identity-less caller, which is F-5/H-8 | `graph/mcp.rs:520-524,632-706`, `offload/mcp.rs:400-414` |
| M-9 | The `#46` outcome split covers the manifest fetch but not the **artifact** fetch: any asset 404/timeout is recorded as a bundle *rejection* (red card, `unreachable_streak` reset). The deploy note's own publish order makes this the likely steady state. | **FIXED** `a17f25c` — an artifact fetch failure is `Unavailable`, not a rejection | `updater/mod.rs:1032-1039,1059` |
| M-10 | A crash *during* rollback deletes the files the rollback already restored — the journal has two phases and the rollback is an unrepresented third state. Permanent, uncarded, and `warn!`s "the previous version was restored". | **FIXED** `a17f25c` — `Phase::Restoring`; recovery no longer deletes what the rollback restored | `updater/mod.rs:1423-1434,1365-1370` |
| M-11 | `restore_archived` swallows per-file failures; the caller then reports "the previous version was restored" verbatim, and `healthy` cannot see missing files. Silent permanent coverage loss with a reassuring message. | **FIXED** `a17f25c` — restore debt is durable and retried; verdict = health AND no debt | `updater/mod.rs:1401-1413` |
| M-12 | Crash recovery only runs when the updater is enabled *and* something is due. A user who turns detection off after a crash strands a short/empty `rules.d` permanently — "never degrade to no rules" fails closed on an unrelated switch. | **FIXED** `a17f25c` — recovery no longer gated on enabled-and-due | `updater/mod.rs:1974-1999` |
| M-13 | An identifier collision between a shipped rule and a user rule freezes the update channel forever and blames the user's file — U-4's exact symptom, in the case the README tells users to expect. | **FIXED** `a17f25c` — the user's rule is RENAMED (`custom_`), not dropped; **U-4 amendment WRITTEN 2026-08-11** (Phase C3 §"U-4 amendment 2026-08-11"; the stale "still fails and still rolls back" sentence marked SUPERSEDED in place). Verified while writing it: forgiveness now keys on the `local/` prefix **alone**, not "was failing before" (`updater/mod.rs:461-471`), only a *bundle* failure vetoes, an *introduced* `local/` failure is reported and deliberately NOT rolled back (`:472-513`), and a rename is a notice not a fault (`renamed` sits outside `Status::healthy`, `signature.rs:159-164`). Two copies of the retired claim also corrected (`MAINTENANCE.md`, and a live-verify **negative control that asserted the behaviour M-13 reversed**) | `updater/mod.rs:430-437` |
| M-14 | The updater's run lock is process-local; two instances sharing an exe directory race the swap and can destroy the old bundle with the journal pointing at an empty archive. | **FIXED** `a17f25c` — `update.lock` via `create_new`, age-based staleness, never pid | `updater/mod.rs:490`, `store.rs:69` |
| M-15 | H-1's gate-cache fix narrows the race but does not close it: the epoch is bumped *before* the beacon POST and never after it resolves, so a query issued **during** the POST caches an `open` verdict for a full 2 s TTL. | **FIXED (the whole in-flight window is now a refusal, not just an un-cached verdict)** `d1cfc0b` — see the M-15 note below | `tabs/config.rs` (`opencode_plugin_source`: `CIMP_WEB_PENDING`, `cimpGateState`'s `settle`, the gate's `external`) |
| M-16 | `read` is deliberately left unpinned to preserve OpenCode's `*.env` → "ask" carve-out — but last-match-wins applies to the *project* config too. A repo shipping `{"permission":{"read":"allow"}}` resolves `read * → allow` and `.env` is read with no prompt. Verified live. | OPEN | `config.rs:2038-2043` |
| M-17 | `/mcp/call` and worker error paths carry the remote MCP server's `error.message` verbatim and up to 300 chars of raw response body — unscreened, unwrapped, unbudgeted — while both call sites' comments assert these are cImp-composed strings. | OPEN | `mcp_host.rs:1282-1314`, `loopback.rs:3804` |
| M-18 | The SSRF widening reads CIDR notation and doc placeholders as fetch targets: `"RFC1918 (10.0.0.0/8)"` in a *search query* refuses the whole call with a security error. Compounds — each benign denial raises the power-of-two threshold that suppresses a later real one. | OPEN | `outbound.rs:386-415,660-684` |
| M-19 | Decision 21's "empty is not absent" reasoning is applied on the headless path but not the loopback path: a `PersistentWrite` with no resolvable tab identity is stored **unquarantined**, and `scoped_session` attributes it to another tab's session. | **FIXED (both defects; quarantine — not refusal — plus a write-only session resolver)** `19440d2` — see the M-19 note below | `loopback.rs` (`unattributed_write`, `LatchRegistry::gate`), `toolclass.rs` (`WriteTaint::Unattributed`, `write_notice`, `UNATTRIBUTED_WRITE_NOTICE`), `graph/mcp.rs` (`write_session`, the `context_note` arm) |
| M-20 | `context_note` text is unbounded and the secret screen only sees the first 256 KiB — 256 KiB of filler then an AWS key stores Clean. `secrets.rs:120-128` asserts the opposite ("it cannot reach either bound"). | **FIXED (bound made structural; the false claim corrected)** `19440d2` — see the M-20 note below | `graph/secrets.rs` (`NoteText`, `MAX_NOTE_BYTES`, the static assert, `screen`), `graph/mcp.rs` (the `context_note` arm's parse boundary) |
| M-21 | A worker-only detection override leaves the updater inert and the manual buttons lying: `updates_enabled` resolves at `Scope::App`, which deliberately excludes the worker row, so the UI says "detection is off" about a layer that is running. | OPEN | `updater/mod.rs:246-252`, `injection.rs:724-729` |
| M-22 | The override popover renders a click-time snapshot that never updates while open — a tab that becomes contaminated while the user reads it keeps saying "Not latched." | OPEN | `TabBar.svelte:68-102` |
| M-23 | Promote (unquarantines attacker-authored text into future sessions) is one unconfirmed click; Discard (which can only lose a note) is behind a modal. Polarity inverted vs. the latch UI, which gets it right. | OPEN | `CodeIntelligenceView.svelte:338-339` |
| M-24 | `Unscreened`, detector flags, `MemoryQuarantine` and `LatchOverride` collapse into one red chip; only denials are visually distinct. "We did not look at all of it" reads as "we blocked something" — the opposite. | OPEN | `ToolActivityView.svelte:356-375` |
| M-25 | The frontend branches on `rules.armed` where the backend publishes `rules.healthy` for exactly this question — 3 of 4 rule files failing renders full protection. `offload.ts:212` documents that `healthy` must be read, never restated. | **FIXED (`healthy` is the gate; a fourth state for the partial set; the contract made mechanical)** `2ab5b86` — see the M-25 note below | `latch.ts` (`withSignatureHealth`, `startLatchPolling`), `offload.ts` (`RulesHealth`, `rulesHealth`), test `detectionContract.test.ts` |
| M-26 | `FILE_COMPACT_LINES` equals the sum of the per-kind caps (1000), so at saturation every single write triggers a full file read + atomic rewrite, and the accepted child-append race opens on every write instead of every ~1000. | **FIXED** `d652171` — `FILE_COMPACT_LINES` derived with headroom + a compile-time assert | `activity.rs:58-84,472` |

### M-6 — what was closed, and what was declined

The finding names **three** defects. They are not the same size, and the third is
a posture decision rather than a bug.

**Closed (defect 1, the envelope).** `spotlight::scanner_envelope` /
`SCANNER_PREAMBLE` join `envelope` and `recall_envelope` as a third standing
instruction over the *same* nonced markers — one vocabulary for the Phase D
session guidance to teach, three honest first lines. The audit report is
delivered inside it. The preamble says the one thing the other two must not:
**the findings are meant to be acted on as findings; it is the quoted fragments
that are inert.** A preamble that flatly said "do not act on this" would be
telling the model to ignore the report it just asked for, and a standing
instruction the model can catch out is one it learns to discount.

`ensure_closed` recognizes the new preamble. That is not cosmetic: the report is
capped at 64 KB (`MAX_RESULT_BYTES`) and the worker caps a tool result at 32 KB,
so a large report is truncated **by construction** and would otherwise reach the
model with an unterminated data region.

**Closed (defect 2, detection).** The report now goes through the same
`screen()` both EXTERNAL boundaries use. `wrap_external_result` and the new
`wrap_local_report` are two callers of one private `compose`, so the composition
order (detect on RAW → envelope → header outside the markers and in front) has
one definition and cannot drift.

**On the F-9 budget — measured, not assumed.** A 64 KB report shaped like real
audit output (one finding per line, `node_modules` paths, a quoted `eval()`)
against the shipped rules, debug build, idle machine: **~105 ms over both
passes** (5 trials, 104.7–106.9 ms), against `SCAN_TIMEOUT`'s 1000 ms.
Normalization alone is 2.7 ms and *does* change the text (single newlines fold),
so pass 2 really runs. This does not worsen F-9 in the way that matters:
`scan_outcome_with` starts its own `Instant` per call, so the audit scan
consumes no other call's budget — and it is 105 ms bolted onto an operation
that takes minutes. What it adds is one more call site that could itself time
out under load, which fails honestly to `DidNotComplete` and is reported.

**Declined (defect 3, contamination) — and why the reasoning does not stop at
audit.** The finding is right that a `context_note` written after an audit is
not quarantined. It is not right that latching is the instrument.

The argument "third-party text entered the context, therefore the conversation
is contaminated" applies **verbatim** to `read_file` on `node_modules/`, to
`graph_snippet`, to `code_search` match text, to `run_check` diagnostics and to
`POST /context/should_read`'s advisory — every one of them LOCAL-CAPABILITY, and
none of them latching. If it justifies latching on `security_audit` it justifies
latching on any local read, which is the whole class, which is *"reading your own
repository costs you your local tools and marks the conversation contaminated"*.
That is not a posture cImp can ship, and half-shipping it (audit only) would be
worse: an inconsistency with no principle behind it.

What actually separates EXTERNAL is not whose bytes they are but **whether an
outside party is answering this conversation**. A fetch is a request to a party
outside the trust boundary that composes a reply *for this turn*; a scanner
quotes bytes that were already on disk, chosen by whoever put them there, with
no knowledge that this conversation exists. The envelope is the right instrument
for *untrusted text in a trusted channel*; the latch is the instrument for an
*untrusted counterparty*. Note the finding's own comparison supports this:
`context_recall` is enveloped and **also does not latch**.

The residual is therefore recorded, not closed: **a note written after an audit
carries unquarantined text the model may have read out of a dependency.** It
belongs with `graph_snippet`, which has the identical property over a much larger
surface, and so with H-1's standing question about the structural graph tools —
not with a one-off latch change here.

**Scoped to audit, deliberately.** `run_check` output and `POST
/context/post_edit`'s check diagnostics are the same shape, and
`scanner_envelope`'s preamble is written to serve them ("code scanners run over
this project's files"). They are *not* wrapped in this pass because each has
several dispatch paths with different scope resolution — `run_check` alone has
three (`graph::service::run_graph_tool` with its `CallGuards`, the headless
`graph::mcp` path at `Scope::App`, and `offload::tools::run_check` at the worker
scope) — and a marker that appears on one path and not another teaches the model
the marker is decorative. Doing it needs all paths in one change, with the
`CallGuards.spotlight_recall` field renamed to what it actually is. That is a
follow-up, not a footnote.

**How the call sites are pinned, and the residual there.** `run_audit` returns a
`RawReport`, not a `String`, and `RawReport`'s field is private to `audit::mcp`;
`deliver` is the only way any other module can obtain the text. Deleting either
consumer's `.deliver(…)` is a **compile error**, verified by doing it — this is
deliberately not the `/audit/run` gate's shape, where the test exercised the
helper and the call site was deletable with the suite green. `Delivery` carries a
`Scope`, not two pre-resolved booleans, so there is no `false` for a call site to
hardcode: the only way off is the user's own setting, resolved through
`effective()`. The residual is stated rather than hidden — **inside
`audit::mcp`** the field is reachable, and `run_audit` does reach it once, to put
the *raw* report on the Tool Activity row, which is a human surface and must not
carry markers (the reason `recall_envelope` skips the Memory UI).

### M-7 — what was closed, and what was not

**Closed (hazard 1, the taint gate).** All three routes now resolve a latch scope and
gate before they reach `GraphService`, in `/graph_run`'s shape. The gate needed an
identity to resolve, so `--tab <id>` is baked into the `--precompact-hook`,
`--read-hook` and `--postedit-hook` commands at spawn (as `--context-hook` and
`--taint-beacon` already were), the three shims forward it, and the generated
OpenCode plugin sends `CIMP_TAB_ID` on its `post_edit` POST. Under an EXTERNAL latch,
`post_edit` no longer runs the project's checks and `should_read` no longer returns
source text; each answers with its own fail-safe (empty text / verdict `pass`), so no
hook can perturb the turn it observes.

They gate on a new `LatchRoute::Hook`, whose one rule is **a hook may be refused by a
latch but must never move one**. Gating them as elective calls would have latched every
tab with the read advisor or auto-check enabled to `Local` at its first read or edit,
silently refusing every proxied web/MCP tool for the rest of the session — a worse
regression than the hole.

**Closed (the enumeration).** The three routes did appear in the pinned path list; what
they appeared in was an enumeration of *strings*. `ROUTE_CONTAINMENT` now records, per
route, whether it gates and — for the fixed-tool routes — whether an EXTERNAL-latched
conversation is refused there, computed from `toolclass` rather than restated. The two
route tests read the same list.

**NOT closed (hazard 2, the caller-supplied `cwd`).** `post_edit` still runs the user's
vetted check commands in whatever directory the body names, and a caller that simply
omits `tab` is not gated at all (the locked fail-open of `latch_scope`, tracked by
F-5/H-8). Any narrowing keyed on the caller's own `tab` is walked around by omitting
it, so the only sound closures are (a) fail-closed without identity — an F-5 decision —
or (b) an allowlist of roots cImp will execute in, derived from configured tabs rather
than from the request. (b) is H-7's shape and is **folded into H-7's pending
V32-vs-V33 decision** rather than half-built here.

**Adjacent, recorded not fixed.** `POST /context/retrieve` is ungated for a stated
reason (auto-injection, contained by the spotlight/quarantine envelope), but its digest
carries exported signatures — the same content H-1 demoted `graph_repo_map` for. The
`ROUTE_CONTAINMENT` row says so; it is a standing question, not a settled one.

**Two residuals found in orchestrator review of the fix, neither a regression.**

1. **`Hook`'s non-engagement leaves the read-then-fetch ordering open for `post_edit`.**
   `post_edit` returns check diagnostics — which quote source — into the context while
   the latch stays where it was, so a later `ddg__fetch_content` can carry that text
   out. Under `LatchRoute::Native` the equivalent (`run_check`) would have engaged
   `Local` and blocked the fetch. This is *pre-existing* (the route was previously
   ungated **and** non-engaging), and the alternative is the session-killing regression
   `LatchRoute::Hook` exists to avoid. A narrower option, if this is judged worth
   closing: **engage only when `post_edit` actually returns non-empty check output** —
   a no-op in the default config (`graph.auto_check` defaults off, so `post_edit`
   returns `None`) and containment restored when it is on. Costs a user with auto-check
   enabled their web tools from the first edit that produces findings, which is why it
   is a decision and not applied.
2. **`hook_*` are the first `TABLE` rows that are classified but not dispatchable.**
   A model emitting the bare name `hook_post_edit` on `LatchRoute::Native` now
   classifies LOCAL-CAPABILITY and **latches the tab to `Local` before dispatch rejects
   the name** — the A-1 harm (one hallucinated name costs a tab its tools) in the
   direction the A-1 fix did not cover. Low probability (the name is in no tool list),
   but it is exactly M-2's subject: **M-2's tripwire must assert `TABLE` ↔
   dispatchable-name correspondence, with the three `hook_*` names as a documented
   exception.**
   **Closed by M-2** (`f895e74`), with one correction to this paragraph: they
   were **not** the first such rows — `Edit`/`Write`/`Bash` have been classified and
   unroutable since Phase A, and `Bash` is the likelier hallucination of the six. The
   exception is now a declared field (`ClassRow::dispatchable`) checked against the
   real dispatch surface, and the gate leaves the latch alone for all six rather than
   only documenting them.

### M-2 — what was closed

**The A-1 fix stayed.** A hallucinated bare name still neither latches nor is
refused; `a_hallucinated_bare_tool_name_does_not_latch_the_task` and
`an_unserveable_name_on_the_native_route_does_not_latch_the_tab` are untouched
and still green. What was missing was the invariant that fix started depending
on, which nothing stated and nothing checked:

> **Every native tool name a dispatcher can serve has a `toolclass::TABLE` row,
> and every `TABLE` row no dispatcher serves says so.**

**It is now mechanical, in both directions, from ONE assertion.** `ClassRow`
grew a `dispatchable` flag (`row(…)` sets it, `unrouted(…)` clears it — the
default is the safe one) and
`table_matches_the_native_dispatch_surface` compares `TABLE`'s dispatchable rows
against a set **derived by scanning the dispatchers' own source**: the match arms
of `offload::tools::dispatch` and `graph::mcp::run_tool`, the `name == "…"`
chains in `dispatch_recorded` / `handle_call` / `run_graph_tool`, and the
literals of `loopback::offload_tool_name`. A hand-written list compared against
itself is the tripwire shape this finding is about, so there is not one.

**The scanner is itself hand-written, so it fails loudly rather than quietly.**
Every line at match-arm indentation must be a string-literal arm, a declared
`NON_NAME_ARMS` entry (the unknown-tool catch-all; the `graph_` prefix
*delegation*), or a comment/closing delimiter — anything else panics with the
line. A site that yields **no** names panics too, so a scan that has drifted
from its source cannot pass vacuously. And
`every_advertised_tool_is_classified_and_dispatchable` cross-checks the same
property at *runtime* against the real spec builders (`graph::tool_specs`, the
two semantic specs, `audit_tools::defs`, `tools::enabled_defs`), which is what
catches a dispatch arm the scanner stopped recognizing.

**Item 2 — the latching half — was fixed, not just documented.** The three
`hook_*` rows M-7 added were *not* the first classified-but-unroutable rows:
`Edit`/`Write`/`Bash` have been there since Phase A, and `Bash` is the live
case, because a local code model reaches for it out of habit. All six
classified LOCAL-CAPABILITY or TRUSTED, so A-1's rule never saw them: they
**engaged the latch and only then met "unknown native tool"** — A-1's harm in
the direction A-1 did not cover. `LatchRoute::can_execute(name, class)` now
carries both rules for both gates, and the argument for widening the
wave-through set is that it is the *same* principle, not a new one: on a native
route an EXTERNAL classification already means "no dispatcher will serve this",
and these six are the rows where that default does not apply. The risk — a
wrong `dispatchable: false` waving a real capability past the latch — is
exactly what the tripwire above measures, and the constructor makes the safe
value the one you get by default.

**What it deliberately does not touch.** `can_execute`'s second rule is confined
to `LatchRoute::Native`. `Hook`'s name is composed by cImp and *is* the route,
so applying it there would wave through the gate M-7 built —
`can_execute_covers_the_unroutable_names_without_reaching_the_hook_routes`
asserts a contaminated tab is still refused `/context/post_edit`, next to the
same name arriving as a model's tool call and being ignored. `Proxied` is
untouched: every id there contains `__` by construction, so unknown-⇒-EXTERNAL
still governs every name that can carry external content.

**One M-7-era test was pinning the defect.**
`a_hook_route_reads_the_latch_and_never_engages_it` used the *same name on
`LatchRoute::Native`* as its control and asserted it latched — the behaviour
M-7's own residual note called out as M-2's subject. The control is now a name
that really is elective and really dispatches (`graph_snippet`), and the old
case is asserted the other way round beside it.

**F-10 came in with it, as a shared function *and* a compile error.** See the
F-10 entry.

### M-8 — what was closed, and on what discriminator

**The literal finding is a no-op and was not the fix.** `run_check` is already
`ToolClass::LocalCapability`, and the only gate it dispatched above
(`headless_write_refused`) refused PERSISTENT-WRITE and nothing else — so moving the
`run_check` block below it changes no behaviour. The real hole is that the headless
path has **no latch at all**, by design, and therefore serves the whole
LOCAL-CAPABILITY class unlatched: `run_check` (executes the project's configured
commands) plus the six graph tools that return source text. `run_check` is the
sharpest instance, not the extent.

**Closed.** `handle_call` now takes the calling MCP child's `--tab` identity and
consults one class-driven gate (`graph/mcp.rs::headless_refusal`) as the first thing
it does — above the `run_check` dispatch and above the index open. PERSISTENT-WRITE
is refused as before; LOCAL-CAPABILITY is refused **when the child serves a cImp tab**
and served when it does not.

**Why not the `ProxyMiss` reason.** Gating on *why* the proxy missed — allow under
`Transport` ("the app is genuinely unreachable"), refuse under the
attacker-manufacturable ones — narrows nothing, because **every** reason is reachable
with a single `Write`, the tool decision 3 concedes cImp cannot gate. Truncating
`.cimp-discovery/<pid>.json` gives `NoInstance`; *adding* a well-formed entry whose
`root` is a deeper ancestor of the child's cwd and whose `port` is dead gives
`Transport`, because `loopback::select_discovery` prefers the deepest matching root
(`loopback.rs:170-195`). A gate keyed on the reason is a gate the attacker holds the
key to. This contradicts the enum's own doc comment, which calls `Transport` "the case
the fallback was designed for" — true of intent, false of reachability.

**Why `tab` is the discriminator.** It is argv, baked in by cImp at spawn on both
consumers' paths and unreachable from any request body. `Some` ⇒ a latch for this tab
exists in the app and this path cannot read it. `None` ⇒ the child was not spawned by
cImp: the documented first-class headless consumers (`claude -p`, cron —
`ARCHITECTURE.md` § *Warm pool vs. fallback child*) have no tab, no latch scope
anywhere, and are **ungated on the app path too** (`latch_scope`'s locked fail-open).
The restored invariant is therefore: *the headless path is never more permissive than
the app path would be for the same caller identity*.

**Residual — narrowed, not eliminated.** An attacker who can make the child present
**no** tab identity still gets unlatched LOCAL-CAPABILITY headless. cImp's own children
cannot be made to do this (argv is fixed at spawn and reaches this frame without
touching a request body), but a caller that can **spawn processes** can simply start
its own MCP child with no `--tab`.

That bypass is **self-limiting, and stating why is the point**: it requires exactly the
capability the gate is protecting. Anything able to spawn a tabless child can already
run the project's checks and read its source directly, so the bypass grants it nothing
it did not already hold. The gate therefore has teeth precisely where it needs them —
a caller with no process-spawn capability (the offload worker; a tab whose harness
denies `Bash`/`run_command`) cannot reach around it. Do not "discover" this later and
file it as a hole; it is the boundary the design accepts.

Beyond that, the residual is the same one F-5/H-8 track everywhere else: *an
identity-less caller is ungated by design*, now with one more consumer riding on it. The second, accepted cost is a false
positive: a tab whose latch is `Open` also loses these tools while the app is
unreachable — the only direction available to a frame that cannot read the latch, and
an already-anomalous window (an AI tab is a cImp webview; cImp down normally means the
tab is gone too).

---

### M-25 — H-10's other half, and why the fix is a fourth state

**The defect is a lying indicator, not a wrong field name.** `armed` is
`files_loaded > 0 && rules > 0` — *can this rule set match anything at all?* —
and the question every one of these surfaces asks is `healthy`, *is the rule set
on disk live?*. A `rules.d` of four files with three failing to compile is
`armed` and not `healthy`: `scan` runs, matches on a quarter of the signatures,
and returned **silence** to the status chip, the tab badges and the taint
popover — silence being exactly what full protection looks like there. A user
who checks the indicator before pasting a fetched page reads that as covered.
H-10 closed the *unknown-vs-armed* half of this; this is the *partly-healthy*
half, and it is the worse direction: a confident wrong claim rather than a
missing one.

**Fixed by moving the question, not the field.** `withSignatureHealth` gates on
`healthy`, and the read arm of `SignatureHealth` now carries the backend's whole
`RulesHealth` verdict (`armed`, `healthy`, `files_failed`) instead of one boolean
this side chose to keep — a field that is not on the union cannot be branched on
by mistake, which is the H-10 lesson applied one level up.

**Why a fourth state and not a fourth reuse.** Rendering the partial set as the
existing disarmed row would have told the user "the rules directory compiled to
no usable rules" about a layer that *is* matching, and counted it as *switched on
but inert* in the chip's sentence. That is the same family of lie in the
understating direction, so the row carries `partial` alongside `unknown`, and the
distinction survives to the pixels exactly as H-10 required of the third state:
its own word in the popover (`partial`), its own sentence on the tab ("Running on
only part of what it needs: …"), its own `reducedCounts` bucket and its own
clause in the chip tooltip ("1 layer only partly loaded"). The row names the
number of failed files, so the user is sent to the files rather than to a switch
nobody flipped. The chip stays on the confident word `reduced` and stays
un-degraded: `unverified` means *nobody could read this*, and reaching it with a
perfectly-read partial set would understate a known loss of coverage. **No
`.svelte` file needed a change** — every render decision already lives in
`latch.ts`'s pure functions, which is the G-2 arrangement paying for itself.

**The contract is now mechanical, which is the actual finding.** `offload.ts`
already said `healthy` was the field to read and that it must never be restated;
`latch.ts` read `armed` three lines under a comment pointing at that rule. A
documented contract with no enforcement is a comment. So: `rulesHealth()` in
`offload.ts` is the one extractor, and `src/lib/detectionContract.test.ts` scans
every shipping `.ts`/`.svelte` (via `import.meta.glob`, since the app tsconfig
has no node types) and fails the suite on any read of `rules.armed` /
`rules.healthy` outside a two-entry allowlist — `offload.ts` (declares the type,
owns the extractor) and `SettingsApp.svelte` (the panel whose job is to render
the raw read: `healthy` drives the N-3 dot, `armed` adds the "nothing to match
with" clause). It also asserts both allowlisted files still read it, so a stale
exemption cannot be inherited, and that the scan found the tree it meant to —
the failure mode of every source scan is passing by finding nothing.

**A test pinned the defect, again.** `latch.test.ts`'s *"says nothing when the
layer is armed"* asserted silence for `{ armed: true }`, which is true of the
3-of-4-broken directory. Rewritten as *"says nothing only when the WHOLE rule set
on disk is live"*, paired with the finding's own case and with a
partial-vs-inert-vs-unreadable test in the shape H-10's pairing established.
**RED-checked per mutation**: restoring the `armed` gate fails 5 tests; keeping
the `healthy` gate but collapsing `partial` into the inert row fails 3;
re-introducing a hand-lifted `rules.armed` at the poll site fails the contract
scan with file and line. The rewritten silence test is green under the first
mutation *by construction* (a live set is also armed) and RED under removal of
the silence gate — its partner carries the finding.

**Checked and clean, report only:** `SettingsApp.svelte` was already correct
(N-3's dot binds `healthy`; its `armed` clause is a diagnostic beside the file
counts, not a protection verdict). It is the only other frontend consumer of the
detection status.

### M-15 — what was closed, and why the epoch could never have closed it

**The race is real and the finding's mechanism is exact.** H-1's epoch catches a
query that is *already in flight* when a beacon fires. It is structurally unable
to catch the other one: a query issued **during** the beacon POST starts at the
already-bumped epoch, and the reply it gets is not stale at all — the app has
genuinely not been told yet, because the POST that tells it is the one still in
flight. Nothing about that verdict looks suspect, so `settle` cached it, and a
`read` issued after a `webfetch` was admitted for a full `CIMP_GATE_TTL_MS`.
Bumping the epoch a second time *after* the POST would not have closed it either:
the racing query can commit before that second bump lands.

**Fixed by making the window itself a first-class fact.** `CIMP_WEB_PENDING`
counts native web calls of this tab that have been **admitted but not yet
reported**. It is opened on the statement immediately after the epoch bump and
closed in a `finally`, so it covers the POST, the disabled-beacon `return` and
every throwing/aborting path. Two readers, and both only ever tighten:

- **`settle` will not cache across it.** The query snapshots the counter at its
  start; together with the epoch that covers *every* beacon overlapping the query
  — one that starts mid-query moves the epoch, and one that started earlier was
  by definition still pending at the snapshot. Two integers read at one instant
  decide "did contamination touch my window".
- **the gate treats it as an EXTERNAL latch**, and reads it at the *deny site* so
  no cache path can bypass it — cached, fresh and fail-open verdicts all pass
  through the one expression. Only the LOCAL direction consults it; folding it
  into the web direction would relabel a `local` latch as `external` and turn
  that line's refusal into an admission, which is the one way a local signal
  could have *loosened* the gate. A test asserts that inversion never appears.

This is decision 17's trust rule applied literally: authority to refuse **nothing**
still comes from the app (`gate: true` is the app's word and nothing local can
synthesize it); a signal that can only tighten is safe to take from a weak,
in-process source.

**`settle` also stopped answering `open`.** It now caches conditionally and
returns the app's verdict unconditionally. Two reasons, and the first is a bug
this fix would otherwise have had: `open` carries `gate: false`, so a query that
answered with it skipped the deny site's `st.gate === true` guard entirely and
admitted the very call the window exists to refuse. The second is that it was
never load-bearing — the latch is **sticky** app-side (it only tightens and never
re-opens), so any verdict that comes back is a *lower bound* on the real
restriction. Applying one late can under-refuse, never over-refuse. Caching it is
the only dangerous act, and that is what is now conditional.

**Cost, measured rather than asserted** (node 25, 200 000 gated calls per arm,
this plugin before and after the change, two runs):

| | before | after |
|---|---|---|
| hot path, warm cache | 162.9 / 161.6 ns/call | 154.2 / 156.7 ns/call |
| `/latch/state` round trips for 20 reads issued during one beacon POST | 1 (19 served from a pre-contamination cache, **all 20 admitted**) | 20 (nothing cached, **all 20 refused**) |

The common path is unchanged — an integer compare, inside measurement noise, and
the cache still serves 200 000 calls off one query. The whole cost is one extra
loopback round trip per local tool call issued *while a beacon POST is in flight*,
which is bounded by that POST's own `AbortSignal.timeout(2000)` ceiling and is in
practice a sub-millisecond in-process POST.

**Tests, and what they would still pass with.**
`the_beacon_window_opens_beside_the_epoch_bump_and_always_closes` is the
structural half: exactly one open and one close, the close in a `finally`, and —
after stripping comments — **no `await` between the epoch bump and the window
opening**, which is what makes the adjacency a guarantee rather than an ordering
(the engine is single-threaded, so with nothing awaitable between them no query
can start in the sliver where the epoch has moved and the window has not opened).
`the_gate_refuses_local_tools_while_the_beacon_is_in_flight` is the executable
half — a second node-driven interleaving beside H-1's, with the beacon POST parked
and released by the driver, no sleeps and no timers anywhere, so it is
load-insensitive by construction. It admits a control `read` first (a plugin that
refuses everything cannot pass), compares both refusals against the shipped
`REFUSAL_NATIVE_LOCAL_BLOCKED` constant rather than merely catching (a `TypeError`
is not a pass), and asserts a **new** `/latch/state` query after the beacon lands,
which is what distinguishes "the cache was left empty" from "the window never
closed".

**RED-checked per hunk:** reverting the deny-site clause fails the executable test
at *"admitted a local tool while a web call was in flight"*; reverting `settle`
fails it at *"admitted after the beacon landed"* — the finding's own sentence;
removing the window fails both source and executable tests. **One honest gap,
reported not hidden:** breaking only the `finally` decrement leaves the executable
test GREEN (a leaked counter refuses correctly forever), and only the structural
test catches it. That failure mode is a lockout of the tab's local tools, not a
containment loss, and the deterministic guard for it is the source assertion; the
alternative — asserting a cache hit within the 2 s TTL — would have been the one
timing-dependent line in the file.

**CI:** `.github/workflows/tests.yml`'s ignored-test inventory goes 3 → 4 and the
node-backed step now runs both gate tests by name, one `--exact` run each (O-1's
"a filter that matches nothing exits 0" guard applies per name).

### M-19 — two defects, both closed, and which precedent each followed

The finding names two things that are not the same size. Reported separately
because they were fixed in different places, for different reasons.

**Defect 1 — the unquarantined store.** `LatchRegistry::gate`'s identity-less
fail-open returned `WriteTaint::Clean` for *every* class, PERSISTENT-WRITE
included, so a `/graph_run` caller with no resolvable tab wrote a `context_note`
straight into auto-injecting project memory. The precedent for treating that
class as an exception was already in the tree, one module over: on the headless
path the identical two missing facts (no session identity, no taint verdict) make
the same call a hard refusal, on the stated grounds that a note written blind is
*"project-wide, permanent, unattributable AND unquarantined — the
highest-privilege write the memory surface offers."* Two paths, the same missing
facts, opposite answers. `unattributed_write` makes them agree. It is a
**quarantine, not the headless path's refusal** — locked decision 10 governs the
loopback path, and the headless refusal exists because there is no running app to
review a queue in, not because refusal is the better answer. Everything else
about the fail-open is untouched: no other class changes verdict, no latch row is
created, and F-5/H-8/M-8's reliance on it is intact.

**It is a new `WriteTaint` variant (`Unattributed`), not a reuse of
`Quarantined`.** `QUARANTINE_WRITE_NOTICE` states as *fact* that "this session
has used an external tool (web/MCP-server)" — which this path has no evidence for
and which is usually false. A boundary message that invents a reason is how a
model learns to discount boundary messages. `WriteTaint::write_notice()` moves the
choice into the enum, so a fourth verdict is a non-exhaustive-match error rather
than a silent reuse of the wrong sentence; the same discipline that made this an
enum instead of a `bool` in the first place. Gated on `policy.quarantine` only,
never on `policy.latch` — decision 16 keeps the two switches independent, and
this is a quarantine decision.

**Defect 2 — the wrong session attribution.** This is the worse of the two and it
is *not* fixed by flagging the note. `scoped_session`'s most-recently-active
fallback keys on `agent`, and `agent` on the loopback path is the request body's
own `consumer` field — so an unattributable caller **chose** which tab's
conversation its note was filed inside, by naming that tab's agent. Closed with
`write_session`, a resolver used only by the write: the session the caller PROVED,
or none. Decision 21 applied rather than cited — `session: None` does not mean
"scope me to whatever is most recent", it means the live-session registry could
not prove a session for this caller, and a write does not get to guess. The
pinned/unpinned split is unchanged (F21): a pinned note is global, so there is no
session to get wrong; an unpinned one is answered honestly instead of silently
misfiled. Reads keep the fallback in full — that is V28's documented pre-upgrade
behaviour and costs nothing.

**Residual on defect 1:** the hold is only as good as the review queue that
receives it, and M-23 (promote is one unconfirmed click, discard is behind a
modal) is still OPEN — an unattributed note is exactly the kind a user should not
be able to promote by reflex. **Residual on defect 2:** a caller with a *real*
tab whose session the registry cannot currently resolve (TTL-stale, or a tab with
no transcript activity yet) also loses the fallback and is told to retry or pin.
That is stated in `write_session`'s doc rather than hidden: `session: None` is
exactly as unproven in both cases, and this frame cannot tell them apart.

**A test was pinning defect 1 as correct.**
`a_local_latch_and_a_tabless_call_both_write_clean` asserted
`Ok(WriteTaint::Clean)` for an identity-less `context_note` under the comment
*"no tab identity ⇒ no scope to latch and none to taint"* — the first half locked,
the second half the defect. Split into `a_local_latch_writes_clean` and
`an_identityless_persistent_write_is_held_not_stored_clean`.
`no_explicit_session_reproduces_the_pre_v28_fallback` pinned defect 2 the same
way (it asserted the note landed in `ses_b`, "the most recent session for the
agent"); it is now reads-only, with the write half stated as its own test.
`an_identityless_call_is_never_gated` used `.is_ok()`, which is true of every
verdict this function can return — tightened to assert the taint per name.

### M-20 — the bound is structural now, and the comment was corrected too

**The finding is exact and the PoC reproduces.** With the fix reverted, a
`context_note` of 256 KiB of filler followed by `AKIAIOSFODNN7EXAMPLE` answers
`Noted (pinned, kept across sessions).` and the credential is in ordinary,
auto-injecting project memory.

**Enforced structurally, not asserted.** `secrets::screen` no longer takes
`&str`. It takes `&NoteText`, a newtype with a private field whose only
constructor caps the input at `MAX_NOTE_BYTES` (64 KiB), which
`const _: () = assert!(MAX_NOTE_BYTES <= signature::SCAN_PREFIX_BYTES)` pins at
or below what the scanner actually reads. The screen is now *unable* to receive
more than it can read — routing around it is a type error, and raising the cap
past the scanner's prefix is a compile error. `parse` takes the `String` by
value, so at the one call site the raw text is moved out of scope and the note
that gets **stored** is necessarily the note that was **screened**. 64 KiB rather
than 256 deliberately: well under the prefix so the normalized second pass keeps
a wide margin against the 1 s scan budget, and the same figure the classifier
already bounds its own input at.

**The false claim is corrected, not merely relocated.** The old comment asserted
the input "cannot reach either bound". Of the two: the **prefix** bound is now
genuinely unreachable, and the comment says so *by pointing at the type and the
static assert* rather than at an assumption about callers. The **timeout** bound
is not proven unreachable and the comment no longer pretends otherwise — it
states the residual (a bounded ≤64 KiB buffer against a 1 s ceiling on a path
that routinely scans four times that), names the outcome if it is ever hit (the
deliberate fail-open every screen here takes), and notes that it is no longer
attacker-*selectable*, because padding past the cap means the note is not stored
at all rather than stored unscreened.

**The other two screens do not share this exposure on this path, and are
disclosed on the path they are on.** `context_note` reaches only the secret
screen — the classifier and the signature layer screen *fetched* content, not
memory writes. On the fetch path both are bounded (`classifier::MAX_INPUT_BYTES`
+ `MAX_WINDOWS`, `signature::SCAN_PREFIX_BYTES` + `SCAN_TIMEOUT`) and both
**report** their boundedness — `Scored.bounded`, `is_bounded`,
`ScanOutcome::DidNotComplete` — so padding there degrades to a disclosed "we did
not look at all of it" rather than a silent Clean. The secret screen was the only
one whose bound had no disclosure channel at all, which is precisely why it
needed one enforced at the input instead. (Whether that disclosure is *honest at
the worker* is M-5, still OPEN, and untouched here.)

**Cost, stated:** a note over 64 KiB is now refused at the parse boundary with a
fixed message and nothing stored. This is argument validation in the same shape
as `require_str`'s, not a security refusal, and it is composed from the constant
so the number in the message cannot drift from the number in the check.

---

## Tripwire gap analysis

The scanner→structural refactors (`62d1110`, `d6408fc`) did what they claimed — the
three `PushNotice` bypass shapes really are compile errors and the injection
switches really are private with zero raw reads outside `crate::settings`. Both are
narrower than the sentences describing them.

| Tripwire | Claimed | Slips past |
|---|---|---|
| Decision 9 / `PushNotice` type | push content is app-composed | A producer added **inside `offload/service.rs`** (private is module-scoped). And the `args` provenance guard was *deleted with* `push_tripwire.rs`: swapping `&snap.root` for a scanner finding message compiles and **both** replacement tests stay green, because `done_snapshot` builds `ToolState`s with empty `findings` vectors — the fixture cannot express the regression. **Two-line fix:** sentinel string in one fixture finding, assert absent. |
| Decision 10 / note-query scan | no module outside `notes.rs` queries the note relation | The scan guards the query's **location, not the filter**. Add `pub fn mem_notes_unfiltered()` inside `notes.rs`, call it from `graph/context.rs` — every guard passes, `mine` goes *up*, suite green, read exclusion bypassed. Same is true today of `mem_quarantined_notes`, a `pub` accessor returning tainted rows with no bound on callers. |
| #44 / private injection switches | every enforcement site resolves through `effective()` | **Five V32-era controls on the same struct are fully `pub` and read raw today** — `session_push`, `detection_update_rules_mode`, `_classifier_mode`, `_interval_hours`, `_manifest_url` (raw reads in `updater/mod.rs`, `graph/service.rs`, `audit/runner.rs`, `oob/mod.rs`). The structural invariant covers Phases B/C/D/F/G/H and *not* Phase C3 or decision 9's own master gate. Privacy cannot express "and the next one". |
| `every_feature_has_a_guarded_l2_field` | every Feature has its own storage | Guards **L2 only**. Writing a new feature's L3 arms as `Feature::NewThing => self.taint_latch` passes every existing test — and a user setting NewThing's per-tab override to `off` silently switches the taint latch off on that tab. |
| Claude `--settings` overlay key-set pin | asserted "against the largest overlay we can emit" | Built from `Settings::default()` (= `sensor`). In `deny` mode a **third** top-level key `permissions` is emitted, so the test never runs against the largest overlay. Part 4's LOW is confirmed still open. |
| `/audit/run` gate | route is LOCAL-CAPABILITY gated | ~~Tests exercise `gate()` and `tool_name_for` **directly**; deleting the `latches().gate(…)` block from `handle_audit_run` leaves the suite green.~~ **Half closed by M-7's fix:** `every_loopback_route_declares_what_it_does_about_the_latch` fails if any route declared as gating stops reaching `latches()`, `handle_audit_run` included (RED-checked against `handle_post_edit`). Still open: the three lines in `audit/mcp.rs` that put `tab` in the body — deleting them produces H-8 and nothing fails. |
| `Screen` wire values / `is_denial` | the variant set is pinned | The same fix run built `declare_origins!` **100 lines below `Screen` in the same file** and did not apply it. The label test iterates a hand-written 10-element array; `Screen` has 11 — `Screen::Unscreened`, the D-1 addition, is missing. `is_denial` is a `!matches!` with fall-through, so a new variant silently defaults to *denial*. |

No dead canary was found: all five V32 detection signals have a live Advisor
consumer. Two have consumer-quality gaps (M-21, H-10).

---

## Prior-findings status at HEAD

| Finding | 2026-08-07 severity | Status now |
|---|---|---|
| C-1 TRUSTED carries private-data tools | HIGH | **OPEN** (H-1) — demotions landed for 5 tools, structural graph tools untouched |
| C-2 forged rotation clears latch + contamination | HIGH | **OPEN** (H-2) — token half closed, filesystem half closed against the PoC only |
| C-3 `/latch/override` model-drivable | HIGH | **CLOSED** — route genuinely absent, IPC-only, `Origin::Ipc` is a fact. Beacon residual documented; but its in-code rationale now argues from "the unfixed H-2", and **H-2 is fixed** |
| C-4 SSRF parser differential | HIGH | **PARTIAL** (H-3) — three named sub-rules landed; the general scheme-spelling family is open |
| U-1 asset-origin traversal | HIGH | **CLOSED** — parsed-URL comparison, every spelling checked. One LOW residual (encoded separators) |
| U-2 archive loop strips rules dir | HIGH | **PARTIAL** — named defect fixed with journal + phase-specific undo; three holes remain (M-10, M-11, M-12) |
| U-3 permanent REJECTED cards | MEDIUM | **PARTIAL** — migration complete and its predicate exact; second call site never split (M-9) |
| U-4 broken user rule freezes channel | MEDIUM | **CLOSED as specified**; collision case remains (M-13). No inverse regression |
| D-1 unscreened ≠ clean | MEDIUM | **PARTIAL** — data model + 3 consumers real, proxy case closed; over-wide exclusion (M-4), false at worker (M-5) |
| D-2 failed reload disarms layer | MEDIUM | **CLOSED** — single swap point, fail-safe, test asserts the invariant. Frontend half regressed (H-10) |
| D-3 budget accounting | MEDIUM | **CLOSED** both sides. Residual: per-attempt reset (M-1) |
| D-4 `scan` blocks a tokio worker | MEDIUM | **CLOSED** — no unbounded spawn, no lost timeout, no dropped result |
| D-5 canary evasion | LOW | **CLOSED** as a documented residual |
| G-1 typo resets settings file | MEDIUM | **CLOSED** for `Override`; does not fail open. Class residual recorded |
| G-2 chip vs raiser predicates | MEDIUM | **CLOSED** — one predicate, backend-published `default_on` |
| G-3 reduced-protection fails silent | MEDIUM | **CLOSED** — `recordPoll` + unknown chip with a live consumer |
| G-4 `/mcp/call` double snapshot | MEDIUM | Docs **CLOSED**, TOCTOU open by decision and honestly recorded |
| H-1 gate cache clobber race | MEDIUM | **CLOSED** — original interleaving closed by H-1, the in-flight-POST one by M-15 |
| H-2 per-tab plugin flags | MEDIUM | **CLOSED**, with migration (not forward-only) |
| A-1 hallucinated name latches | MEDIUM | **CLOSED** functionally; introduced M-2, now also closed (the fix stands, and its invariant is mechanical) |

---

## Documentation truthfulness

`dc3491b` audited only `MILESTONE-V32-injection-hardening.md`, and audited it
against `aed6289` while two further commits landed after. The spec itself came
through well — of 18 previously-false claims, 16 are genuinely resolved and the two
that are not are honestly recorded as open residuals.

**The gap is scope.** `HARNESS-NATIVE-TOOLS.md` — the spec's own named companion
deliverable — was never re-read against the demotions or Phase H, and now carries
three claims that describe closed holes as open and the shipped gate as unbuilt:

- `:422-424` "Structural graph tools, `run_check` and the audit tools keep working"
  — all three are LOCAL-CAPABILITY and refused under an EXTERNAL latch
- `:346-352` OpenCode's natives "remain completely ungated in every mode… Phase E is
  currently unscheduled" — Phase H shipped, those exact eight ids are gated
- `:436-443` the recommended research pattern under `deny` — `offload_task` now
  *engages* the LOCAL latch and is outright refused from the state it is
  recommended for

**Closed 2026-08-09.** The whole document was re-audited against the code at
`0db5739`, not only the three claims above. **Seven** claims were false: those
three, plus (4) the latch bullet also named `read_file`/`code_search`/
`run_command` as things a *tab* loses, which were never on a tab's proxied
surface at all (worker-native tools), and omitted the `graph_struct_search` /
`graph_repo_map` demotion; (5) the exit-path paragraph still said an app restart
was the only clean reset, which `05e613f`'s user-driven clear path replaced;
(6) pre-flight step 5 told the reader to confirm the OpenCode plugin is written
out under `deny`, where `opencode_plugin_wanted` deliberately does not want one;
(7) "every EXTERNAL result is spotlight-enveloped", now a per-scope Phase G
toggle. The file carries a scope note saying what was and was not re-verified —
§§2–4 (harness tool surfaces, live MCP probes) were **not** re-probed and still
carry their 2026-08-07 basis. F-7 is now recorded there as a named residual, so
the H-1 fix is not read as "no signature reaches a contaminated session".

Also: two stale "the unfixed H-2" rationales in the spec and three in `loopback.rs`;
one named test that does not exist (`an_untouched_config_resolves_every_feature_on`,
renamed); a 750 ms scan budget cited in two places where the constant is 1 s
(including in `secrets.rs`, whose header `f31978c` edited three lines away).

**Live verification:** the section is now 24 recipes (1–22 plus 11b, 13b), not 1–15.
**None is recorded done.** Four (7, 11, 13b, 14) were materially rewritten by the fix
run, so any pre-fix evidence for them is void; the rest are textually unchanged but
their subject code moved underneath them.

---

## Supply chain (S-1)

`cargo audit` at HEAD: **16 vulnerabilities**, 0 before V32. All in `wasmtime
38.0.4`, reachable solely through `yara-x` (`wasmtime → yara-x → cimp`). Most are
unreachable at the enabled feature set (Winch, component-model, WASI, pooling all
off); the two not clearly excluded are `RUSTSEC-2026-0006` and `-0087` — Cranelift
**x86-64** miscompilations, the exact backend and architecture shipped.

The process failure is the finding: `yara-x 1.12.0` hard-pins `wasmtime = "38.0.4"`
with no patched 38.x line, there is no `.cargo/audit.toml` entry, no mention of
wasmtime/Cranelift anywhere in `docs/`, and **no supply-chain job in CI** (only
clippy, release, tests; no dependabot).

Posture worth recording explicitly: yara-x compiles rules to WebAssembly and
JIT-compiles that with Cranelift. The process now emits and executes JIT-generated
native code whose input is `.yar` text — from the C3 updater's release and from
user-authored `rules.d/local/*.yar`. The wasm sandbox is *not* the boundary (yara-x
generates the module). And `updater/validate.rs` compiles a staged bundle
**in-process** to gate it, so the un-validated bundle is JIT-compiled as part of
validating it.

All 106 added lock entries are crates.io; no `[patch]`, git, or path overrides.

**Recommend:** pin `yara-x = "=1.12.0"` (the code documents behaviour specific to it),
add a `cargo audit` / `cargo deny advisories` job reading the existing `audit.toml`,
and record the wasmtime exposure as an explicit accept-or-defer.

**Progress 2026-08-09 — the detector exists; the exposure is unchanged.**
`.github/workflows/advisories.yml` runs `cargo audit` from `src-tauri/` (so it
honours `.cargo/audit.toml`) on lockfile changes, weekly, and on demand. It
blocks on the **delta**, not on the absolute count: an advisory against any crate
other than `wasmtime`, a rise above the recorded floor of 16, or `cargo audit`
producing unparseable JSON all fail the job; the recorded set passes with a
`::warning::` and the full table in the job summary on every run. The deferral
carries an expiry (`KNOWN_REVIEW_BY: 2026-10-01`) and the job goes red past it,
so this cannot quietly become permanent.

**Still open, and deliberately not decided by CI:** open question 4. The 16 were
NOT baselined into `.cargo/audit.toml` — that file is the *accepted*-risk log,
each entry carrying a rationale and a revisit trigger, and moving an undecided
exposure into it would be laundering a deferral into an acceptance. `yara-x` is
also still unpinned, and the count is still 16 (verified 2026-08-09 at `0db5739`,
909 crates, plus 23 informational warnings). An upgrade means moving `yara-x` off
its hard `wasmtime = "38.0.4"` pin — no patched 38.x line exists, so the nearest
fixed series are 36.0.13 / 42.0.2 / 43.0.1+, i.e. a yara-x major move, not a
lockfile bump.

---

## Test & CI gaps

- **O-1 — the H-1 regression guard never runs in CI. FIXED 2026-08-09.**
  `the_gate_cache_survives_a_beacon_racing_an_in_flight_query` (`config.rs:4329`) is
  `#[ignore]`d because it needs `node` on PATH. CI ran `cargo test --locked --bin
  cimp` with no `--ignored` pass — **on a job that runs `npx vitest run` twelve lines
  earlier**, so node is unconditionally present.

  `tests.yml` now carries a `cargo test (ignored, node-backed)` step that runs
  it by exact name and asserts `1 passed` (a `cargo test` filter matching
  nothing still exits 0, so the exit code alone would let a rename pass green).
  The other two ignored tests — `tts::engine::tests::synthesizes` and
  `tts::phonemize::tests::espeak_fallback_engages_on_oov` — stay out: both
  **pass** on a machine that has the Kokoro/espeak inputs (verified 2026-08-09,
  all three green locally), and neither input is on the runner, so a blanket
  `-- --ignored` would be red forever. The same step pins the ignored-test
  INVENTORY, so the next `#[ignore]` forces a decision instead of silently
  joining a never-run pile — which is how O-1 happened.

  *Updated by the M-15 fix:* the inventory is now **four** names and the step
  runs **two** node-backed tests, one `--exact` run each —
  `the_gate_cache_survives_a_beacon_racing_an_in_flight_query` (H-1) and
  `the_gate_refuses_local_tools_while_the_beacon_is_in_flight` (M-15). The pin
  worked exactly as designed: adding the second `#[ignore]` failed the step until
  someone decided where it belonged.
- Three tests pin the defect rather than the invariant (see "The pattern").
- Deleting the gate call from `handle_audit_run` or `handle_run` leaves the suite
  green — decision 18's own enforcement has no test at its enforcement point, which
  is the shape #48 explicitly rewrote another test to avoid.
- ~~No test binds `TABLE` to the dispatchable tool set (M-2's backstop).~~
  **Closed** by `table_matches_the_native_dispatch_surface`, which derives the
  set from the dispatchers' own source (M-2's note above).
- No test asserts `/latch/override` stays absent from the router.

---

## Verified correct — do not re-litigate

- **Latch algebra** — bidirectional, sticky, TRUSTED never blocked, PERSISTENT-WRITE
  write-gated under EXTERNAL. A refused call cannot move the latch.
- **Def removal** (invariant 4) — one assembly path, rebuilt whenever the latch
  moves; `force_final` sends `&[]`; `tool_choice:"none"` when the surface empties.
- **Spotlight nonce** — `uuid::v4` from the OS RNG, taken before content is touched,
  never derived from content; a page cannot pre-quote or terminate the envelope.
- **SSRF range predicate** — every locked CIDR and every embedded-v4 spelling exact
  at both boundaries, including `::1`/`::` taken before the v4-compatible unmap,
  NAT64 requiring `segs[2..6]==0`, and 6to4 reading bytes 2..6. Teredo correctly
  excluded. Carve-outs are exact `host:port` from settings only.
- **The memory quarantine is the strongest control in the milestone** — `mem_notes`
  is the single filter; no model-reachable read path leaks a quarantined note;
  write-time taint survives re-index and pin; migration is crash-safe stage-and-swap
  ordered before the schema-version write.
- **Decisions 21 and 22** — headless refusal is class-driven and fires before the
  index opens; the secret screen reuses the same column, read paths and review queue
  rather than adding a parallel mechanism.
- **Decision 9 holds at all three production `PushNotice::new` sites** — every arg is
  a const, an integer count, or a user-chosen filesystem path. The OOB path contains
  no `PushNotice` producer at all.
- **Decision 8's pin is real** and survives a project-config merge (verified live).
- **Phase H is genuinely default-off**, both halves, and "off" is the pre-V32
  behaviour.
- **Settings layer** — G-1/G-2/G-3 closed; `effective()` is genuinely the one
  resolver across 40+ consumers with no TS-side precedence logic; no schema bump is
  correct and every V32 key migrates an existing file to the *protecting* value;
  zero Rust↔TS drift.
- **C-3 is closed** — no HTTP route, IPC-only, contamination survives the override,
  and the frontend cannot be driven to it (no `{@html}`/`innerHTML` anywhere in
  `src/`; child webviews have no IPC capability).
- **`processing/sanitize.rs`** — whole-sequence removal, nested-introducer handled,
  unterminated-string safe, idempotent; re-assembly is structurally impossible.
- **D-2's fix, D-4's fix, D-3's fix, U-1's fix, H-2's fix** — all sound as far as
  their stated scope goes.

---

## Open questions needing a decision (per global principle 10)

1. **H-1's fix shape** — demote `graph_struct_search`/`graph_repo_map`, or strip
   `signature` from `fmt_symbols` for TRUSTED calls? The first costs a research task
   its AST search; the second keeps navigational value. Either way the locked
   decision needs amending, not just the table.
2. **H-6** — signature over the manifest (correct, ~30 lines with `ed25519-dalek`),
   or narrowed workflow permissions + provenance attestation (cheap, weaker)?
3. **H-7** — ship `OPENCODE_DISABLE_PROJECT_CONFIG` default-on for hardened tabs?
   It changes behaviour for users who rely on project configs.
4. **S-1** — accept the wasmtime exposure with a recorded rationale, or block on
   yara-x moving off the pin?
5. **M-18** — the widening's false-positive rate on network/security research. The
   realistic end state is the user switching the SSRF guard off.

---

## Suggested disposition

**Do not release V32 in this state.** The milestone's own definition of done (global
principle 9) requires end-to-end validation on real data, and none of the 24
live-verify recipes has been run — while four of them were invalidated by the fix
run they were meant to validate.

Blocking before release: **H-1, H-2, H-3, H-4, H-8, H-9, H-10** — each is a
containment or detection failure reachable by an injected model with capabilities
the threat model already grants it.

**H-5 and H-6 block the `detection-v1` publish**, not the code release: today the
channel is unpublished, so they are latent. Publishing without fixing H-5 ships a
channel that cannot work; without H-6, one that is authenticated by repo-write
access alone.

**H-7 and M-16** are properly V33 territory (OS-level containment) but should be
recorded as *known-open* rather than left implicit — the current docs present the
additive OpenCode posture as benign.

The doc corrections (`HARNESS-NATIVE-TOOLS.md` especially) are cheap and should land
regardless: it is the document a reviewer consults to learn what the latch covers,
and it currently describes closed holes as open and shipped gates as unbuilt.

### Progress against that disposition (updated 2026-08-08, `5920c92` + `952ae2b`)

The release blocker stands, and the blocking set is now **H-1, H-2, H-3, H-8,
H-9, H-10**. H-4 is closed.

- **H-4 FIXED**, and it was the one blocking item that could be closed without a
  design decision: measured 20/50 hostile probes evading → 0/53, enforced going
  forward by the smoke corpus so the gauntlet rejects any bundle that regresses
  it. **F-2** and **M-4** closed with it.
- **H-5 FIXED and H-6 DECLINED**, so the `detection-v1` publish is unblocked.
  H-6's reasoning is locked decision 28: a signing key usable by the release
  process would live inside the blast radius it is meant to bound, because the
  same `contents: write` publishes the binary. The residual it leaves — narrowing
  `release.yml`'s workflow-level token — is a maintenance item that protects the
  binary release, not a V32 blocker.
- **N-10 PARTIAL**: `coverage_floor` closes the curation half. The hostile-
  publisher half is knowingly left open, for the same reason H-6 was declined.
- **N-8 and N-12 FIXED** as a side effect of retiring the classifier component;
  neither was worth fixing on its own, and both stopped existing.
- **The remaining six HIGHs are unchanged**, and two of them (H-1, H-7) need a
  decision from the owner before code: H-1's fix shape (demote the tools vs strip
  `signature` from `fmt_symbols`) and whether H-7's hermetic OpenCode mode ships
  in V32 or waits for V33.

**Later the same day** (H-1 `0169d10`, H-2 `2c40136`, H-3 `e5f3627`, H-8
`80375a9`, H-9), the blocking set is down to **H-7 and H-10**. H-9's entry above
records the one place where the *proposed* fix was rejected on analysis: a
forensic pin does not survive its own exploit, because the flood vector is
itself forensic. Retention is now per screen, which makes the property
structural — "no source's volume costs another source its history" — instead of
a privilege list somebody has to keep correct. F-3 remains the prerequisite for
the clear path; the lane design does not constrain it (a contamination row is a
new `Screen` variant, and gets its own protected lane by existing).

**On the definition of done, unchanged and still the gating item:** none of the
live-verify recipes has been run. Two were added this pass — recipe 10's
obfuscation variants and the new 11c transport check — and 11c matters most,
because recipes 11 and 11b stage the manifest on a local server that answers 200
and therefore *cannot* catch H-5. A verification step that validates everything
except the property that differs between staging and production is how H-5
survived, and it would have survived the next review too.

---

## Findings raised by the 2026-08-08 fix run (not in the original review)

Each was surfaced by an implementation agent while closing a numbered finding,
verified by hand, and deliberately **reported rather than fixed in place** — a
fix whose blast radius is not reviewable is how the four issue-fixes of the
previous run introduced six defects of their own.

### F-3 — the primary contamination path writes no forensic record at all

`offload/loopback.rs:1750-1752`

H-9 says forensic rows are *evictable*. This is worse and upstream of it: the
main path that sets `contaminated` — an admitted proxied EXTERNAL call — writes
**no activity row**. The only trace is the `info!` at `:1756`, which fires only
when the *latch moves* (`policy.latch && entry.latch.engage(class)`), so a tab
that is already `Local`-latched, or one running with the latch feature off,
contaminates in total silence. There is no timestamp, no tool, no host, no row.

Only the native-web beacon path (`:1815`) records anything, and only once per
tab-session. Consequence: for the common case the system knows *that* a tab is
contaminated and can never say *when*, *by which tool*, or *from which page* —
so contamination cannot be correlated to anything, including a checkpoint.

This is the blocking prerequisite for the user-driven clear path (below).

### F-4 — `is_configured_tab` is agent-agnostic, so `(consumer, tab)` is verified nowhere

`offload/loopback.rs:1026`

The guard checks that a supplied id names *some* configured AI tab, not one
belonging to the asserted consumer. A caller may therefore key a latch under
`("claude", <an OpenCode tab's id>)`. Harmless on `/audit/run` today — a
cross-keyed latch is freshly open and engages a latch nobody reads — but it
means the consumer/tab pair is a *verified* pair on no route. Compounding: the
empty-list escape means that with **zero** AI tabs configured any id resolves to
`Scoped`; low severity (there is no contaminated tab to leak from in that state)
but wider than the doc's "availability floor" framing admits.

### F-5 — `/graph_run` and `/mcp/call` share H-8's tab half

`offload/loopback.rs:2288-2310` · `:3728+`

Both take `tab` optionally with the same `Anonymous ⇒ Ok(WriteTaint::Clean)`
fail-open, and neither logs when the gate does not apply (`/graph_run` does not
even distinguish `Unknown` — it calls `.into_scope()` directly). They do **not**
share the consumer half: their `consumer` selects no capability toggle, only the
latch key via `source_for_consumer`, so an arbitrary string collapses to
`"claude"` and buys nothing.

Not fixed with H-8, deliberately: unlike `/audit/run`, the fail-open here is a
**locked decision** (`latch_scope`'s doc — "a tool call must never fail for lack
of identity"), so closing it is a decision, not a bug fix. Both spawn paths for
those children have carried `--tab` since V28, so the same argument H-8 used
does apply. A shared helper is not obviously right: `/audit/run`'s remedy string
is route-specific and `/mcp/call` reads its consumer from the query string; what
is genuinely shared is a `require_tab`-style refusal plus the `scope.is_none()`
warn.

### F-6 — a silent-degradation path with no consumer (introduced by the H-2 fix)

`oob/claude.rs` — `LiveSessionGate` / `record_names_session`

The H-2 decode proof requires a transcript line carrying a top-level `sessionId`.
If a future Claude CLI drops that field, the tap confirms nothing, forever:
`mark_live_session` stops firing, V28 memory scoping degrades to unscoped and
Usage stops flagging sessions active — quietly, with a green build.
`SubagentState::drift_tick` does not cover this. Wants a canary of the same shape
as the existing drift canaries: *N drains, bytes consumed, zero `sessionId`-
bearing lines.* Filed against the standing rule that every quality signal needs a
consumer — enforce, surface, or delete.

### Scheduled work: the user-driven contamination clear path

H-2 closed the forged reset by removing the only reset, which leaves an app
restart as the sole exit. The owner's decision is that this ships with a
**user-driven** clear instead, on the reasoning that the correct trust root is
not *evidence* (any filesystem fact is forgeable by a model with a shell) but
*authority* — a human acting in cImp's own UI is a fact no shell can fabricate,
the same root the existing consent flows already use.

Agreed shape, to be built as a V32 phase before release, after the remaining
HIGHs:

1. **A contamination event record** at `loopback.rs:1750-1752` — bounded by a
   claim bit like the beacon's, carrying `ts_ms`, scope, tool, host. Closes F-3
   and is the prerequisite for everything else.
2. **H-9's pinning**, so that record is not evictable by 200 `context_note`
   calls.
3. **Checkpoint identity** — thread `session_id` + tab into
   `WorkbenchService::on_prompt` (`/context/retrieve` already receives
   `session_id` at `loopback.rs:2690-2692` and simply does not pass it) and into
   the shadow-commit trailers. Turns correlation from nearest-preceding-by-wall-
   clock into an exact match, and fixes the fact that two Claude tabs are
   indistinguishable in the checkpoint stream.
4. **The clear action** — a third `LatchAction`, an IPC command, a confirm
   dialog, and its own audit row. Lives in the taint popover, **not** only in the
   Timeline: the Timeline is a Workbench section gated on a setting that can be
   off. Must also reset `beacon_flagged`/`latch_flagged`, or a re-contamination
   after a false-positive clear is silent. Must **not** promote already-
   quarantined notes — decision 10 keeps that behind the Memory view's own
   review.
5. **Timeline as the evidence surface** — a row-kind union (the view is fully
   homogeneous today, so a non-checkpoint row is a new concept for it), merged by
   time, with per-row actions.

**The asymmetry to design against, because it runs opposite to intuition:**
restoring a checkpoint rolls back files and **cannot** remove injected text from
the model's context window. So *restore* is the case where clearing contamination
is least justified, while *false-positive resume* is the case where the context
has actually been judged benign. Owner's decision: after a restore the bit stays
set and lifts only when the tap observes a genuine session rotation — which the
H-2 decode proof can now attest honestly. This is the one place a filesystem
signal is trustworthy, because we are waiting for evidence to *arrive* rather
than accepting a claim to *drop* a guard.

Known limits of the design, recorded so they are not rediscovered: checkpoints
are per project root and the Timeline reads the app's launch cwd, so a tab
running in a worktree writes checkpoints the Timeline never lists; and the latch
path records *that* external content entered, never *what* looked suspicious, so
a user judging "false positive" from a beacon row is judging "did WebFetch run",
not "was the page malicious".

### F-7 — auto-injection still pushes source signatures into a contaminated tab

`graph/context.rs:542` (`file_digest`) · `:931` (`public_signatures` → `repo_map`)
· `graph/service.rs:2480` (session-start injection)

Raised while closing H-1, and it **bounds what H-1 can claim**. The fix removes
repo source text from every model-*requestable* TRUSTED path, but the context
auto-injection channel still delivers `signature` lines — file digests and the
session-start repo map — to a tab regardless of its latch.

Not a hole in the same sense, and deliberately not fixed with H-1: injection is a
**push** channel keyed on the *user's* prompt, not on a tool call. The model
cannot steer which file it names, cannot request it, and it crosses no gate by
construction. Cutting it would degrade auto-injection for every clean session to
close a channel the attacker does not control.

Recorded because the inference "after H-1 a contaminated tab never sees a source
line again" is **false**, and that is exactly the kind of over-reading a future
reviewer would make from the H-1 entry alone.

Related, same class, also left alone: `graph/mcp.rs:1568` `semantic_code_query`
prints `signature`, but `graph_semantic_code` is already LOCAL-CAPABILITY and so
already latch-gated.

### Note — the proxy never def-filters the graph surface

`offload/loopback.rs:1530` · contrast `offload/agent.rs:1507,1554`

Not a finding; recorded because it was assumed otherwise while briefing H-1 and
the asymmetry should not have to be rediscovered. The **worker** enforces a
demotion twice — `filter_defs` strips the tool from the advertised list *and*
`latch_gate` refuses it in flight. A **tab** gets only the second: its per-session
child caches `tools/list` at connect, so the graph surface is never latch-filtered
per tab and the refusal at `LatchRegistry::gate` is the whole enforcement.

Security-equivalent (the call is refused either way) but not behaviour-equivalent:
an EXTERNAL-latched tab still *sees* a demoted tool advertised and learns of the
latch by being refused. Pre-existing and identical for `graph_snippet`; H-1's
tests assert the two paths separately rather than deriving one from the other,
which is the right posture regardless.

### F-8 — a denied URL still leaks its hostname to DNS

`offload/outbound.rs` — the resolution step inside `screen_urls`

Raised while closing H-3, and it **bounds what a denial means**. To decide
whether a hostname points into a denied range, the screen must resolve it. That
resolution happens *before* the verdict, so a model can encode data in a
subdomain — `http://<base32-of-secret>.attacker.example/` — and the query
reaches the attacker's authoritative nameserver **whether or not the fetch is
subsequently refused**.

Low bandwidth, and inherent to the design rather than introduced by H-3: any
check of a *name* against denied *addresses* requires resolving it, and this
property has existed since the screen was written. H-3 widens the set of strings
that reach resolution slightly (`http:host` with no slashes now extracts where it
previously did not), which is required by the screen/parser agreement invariant
and is the correct trade.

Recorded because the activity row and the user-facing story both say *denied*,
and a reader reasonably infers "nothing left the machine". That inference is
false. Options if it is ever worth closing: resolve only after a literal-IP
fast-path rejects, cache negative verdicts per host, or route screening
resolution through a resolver whose queries are themselves screened — none
obviously worth it for a channel this narrow, which is why this is recorded and
not scheduled.

### F-9 — the signature scan's 1 s budget is wall clock spanning both passes

`offload/detection/signature.rs:72` (`SCAN_TIMEOUT`) · `:602-634`
(`scan_outcome_with`)

Surfaced 2026-08-09 while verifying the contamination clear path. Under a
saturated machine the shipped-rules scan returns
`DidNotComplete("the signature scan did not complete: timeout")` where it
normally matches.

**Mechanism.** `SCAN_TIMEOUT` is **1 second of wall clock** measured with
`Instant::now()` across *both* of H-4's passes: pass 1 over the raw bytes, then
`remaining = SCAN_TIMEOUT.saturating_sub(started.elapsed())` for the normalized
pass. It is not a CPU-time budget, so a thread descheduled under load burns it
without doing any work. H-4 (`5920c92`) added the second pass without widening
the budget, so the same 1 s now covers twice the scanning.

**Two symptoms, one cause**, both observed: the obfuscation test losing its
second-pass budget (`remaining.is_zero()`), and a benign-page test whose *first*
pass timed out outright.

**Proven pre-existing, not caused by the clear path.** The step-5 working tree
was stashed and the failure reproduced at clean `HEAD` `05e613f` — 1842 passed
/ 1 failed, same test. The module passes **17/17 when run alone**; it fails only
under full-suite load.

**Containment is not bypassed.** The exhausted-budget path returns
`first.merged_with(ScanOutcome::DidNotComplete(...))` and the timeout path
returns `DidNotComplete` — never `Clean`. This is exactly what `ScanOutcome`'s
three-way split exists for, and the detection boundary treats it as incomplete.

**But it degrades where the adversary has leverage, which is worth a decision.**
The layer that is dropped first under load is the *normalized* pass — the
obfuscation defence — and obfuscation is the thing an attacker controls. An
attacker who can make the machine busy (any expensive local operation) raises
the probability that the pass which would have caught them does not run. It
fails honestly, so this is a detection-availability issue rather than a
containment hole, but "the defence thins exactly when someone is pushing on it"
should be chosen, not inherited.

Options: give the normalized pass its own budget rather than the
remainder; measure CPU time instead of wall clock; raise `SCAN_TIMEOUT` now that
it covers two passes; or accept it and make the two affected tests tolerant of
`DidNotComplete` so the suite stops reporting a real property as a failure.

**USER DECISION 2026-08-11 — take options 1 AND 3 together.**

- **Option 1 is the one that addresses the actual harm.** The problem is not that
  scans time out, it is *which* layer dies first: the normalized pass runs on
  `SCAN_TIMEOUT - elapsed`, so pass 1 starves it, and the normalized pass is the
  **obfuscation defence — the thing the attacker controls**. Anyone who can make
  the machine busy raises the odds that precisely the pass which would catch them
  never runs. Giving each pass its own budget removes that asymmetry. Cheap,
  portable, no new dependencies.
- **Option 3 belongs with it as the sizing half**: H-4 added the second pass
  without widening the budget, so the current value was never sized for the work
  it now covers. Alone it only moves the threshold.
- **Option 2 (CPU time) REJECTED** despite being the theoretically clean answer:
  Rust's std has no portable thread-CPU-time API, so it means platform-specific
  code, and — the real objection — it **removes the wall-clock latency bound the
  caller depends on**, so a pathologically slow scan would no longer be bounded
  in real time. That trades a detection-availability issue for a caller-blocking
  one.
- **Option 4 alone REJECTED as the trap**: making the tests tolerant of
  `DidNotComplete` turns a real property into a silenced one, and this family
  already fires in CI on release commits — a check that goes red for known
  reasons stops being read (global principle 3).
- **The tests must still change, as a consequence of 1+3**: re-point them at the
  invariant that matters — **the normalized pass ran, and the outcome is never
  `Clean`** — rather than asserting a specific match that load can legitimately
  defeat. Sizing must be justified against the measured scale below (~105 ms for
  64 KB across both passes), not picked round.

**Separate but adjacent, worth fixing in the same pass:** `release.yml` triggers
on the tag and only `needs: build-windows`, so it **never consults Tests** — a
green tag is not evidence the suite passed, which currently masks exactly this
class of failure.

**One measured data point, from M-6's fix.** A 64 KB audit report (the largest
this new caller can produce) costs **~105 ms over both passes** against the
shipped rules — debug build, idle machine, 5 trials — i.e. ~10% of the budget.
The budget is per *call* (`scan_outcome_with` takes its own `Instant`), so a new
caller never spends an existing one's; what a new caller adds is one more site
that can itself thin under load. Useful as a scale for the options above: the
budget is not tight for realistic payloads, it is tight for descheduled threads.

**Known-flake note for anyone who hits this:** the failing assertion looks like
the H-4 rules regressing. They have not. Run
`cargo test offload::detection::signature` alone before investigating.

---

### F-10 — `NativeRouter` never re-gates the audit and graph tools, so a cloud backend can reach them

**Found by the M-6 fix run; verified independently 2026-08-09. Severity: HIGH.**

`HostRouter::call` (`agent.rs:264-278`) re-gates twice after the scope check —
`graph_*` behind `allow_graph` (V9-01) and `security_audit`/`quality_audit` behind
`allow_audit` (V26) — and both comments say why: *"an unadvertised tool name can still
be **called** by the model, and the audit report is local data that must not reach an
opted-out or remote backend."*

`NativeRouter::call` (`agent.rs:146-156`) implements only the scope check. And the scope
check does not cover these names: a cloud backend's default is
`ToolScope::AllExcept { LOCAL_DATA_TOOLS }` (`schema.rs:2886-2894`), and
`LOCAL_DATA_TOOLS` (`schema.rs:2810-2817`) is exactly
`read_file, list_dir, code_search, run_command, filesystem, git` — **neither the audit
tools nor `graph_*`**. `allows_namespaced` therefore returns `true` for
`security_audit` on a cloud backend, and `NativeRouter` dispatches it: a full local scan
runs and returns repo paths plus quoted source to a remote model.

Reaching it needs the model to emit a name that is not advertised (`enabled_defs` omits
it) — i.e. **exactly the A-1 threat model**, whose base rate this milestone already
measured as non-hypothetical (28 `ok:false` rows in 162 live calls, `a434d4f`).

The root cause is that the defence is written as two `if`s in one of two routers rather
than as a shared function, so `LOCAL_DATA_TOOLS` cannot express it either: `allows`
keys on the segment before `__`, so `graph_*` would need a per-tool entry or prefix
matching — which is why `HostRouter` uses `starts_with("graph_")`. **Fix shape: lift
both re-gates into one function both routers call.** Folded into M-2's task, which is
already about names that classify as capability and reach dispatch anyway.

**FIXED** `f895e74` — `offload/backend_gate.rs`.

All three checks (tool scope, `graph_*`, the two audit names) live in
`BackendGate::admit`, which both routers call and neither may re-implement:
`neither_router_reimplements_the_admission_rules` scans `agent.rs` for
`allows_namespaced`, `allow_audit` and `starts_with("graph_")` and fails if any
comes back. The `allow_graph`/`allow_audit` bools are no longer router fields.

**And skipping the gate is a compile error, not a test.** `tools::dispatch` now
takes a `GatePass<'_>` and reads the tool name *out of it*; only
`BackendGate::admit` can mint one, and it mints it for the name it admitted. So a
native dispatch that never gated has nothing to pass (`E0308`, verified by
reverting), and a pass minted for `read_file` cannot be spent on `graph_snippet`.
The same spirit as M-6's `RawReport`.

Two consequences worth recording rather than discovering:

- **The policy moved with it.** `BackendGate::for_worker(scope, is_remote,
  settings)` is the one place the two opt-ins are resolved, and all three worker
  entry points use it: `OffloadService::run_on`, the headless child
  (`offload/mcp.rs::run_on_backend`) and the supervisor self-test.
  `ResolvedBackend` gained `is_remote` for that — `is_cloud` alone would have let
  a LAN worker reach the index on the headless path while the in-app path denies
  it. What is *advertised* is now derived from the same value
  (`gate.graph_allowed()` / `gate.audit_allowed()`), so a tool cannot be offered
  by one rule and refused by another.
- **The headless child's behaviour changes for unadvertised names only.** It
  never advertised `graph_*` or the audit tools, so the only calls the new gate
  can refuse are the hallucinated ones — which is precisely F-10's threat model.
  A user with `graph.enabled = false` now gets a clean refusal there instead of a
  silent local index read.

**Adjacent, reported not fixed:** `run_check`'s equivalent exposure. Promoted to
its own finding — see **F-12** below, because folding it into F-10's entry would
have let a wider hole inherit a closed finding's disposition.

### F-11 — `select_discovery` is a tamper surface, not just a lookup

**Found by the M-8 fix run. Severity: MEDIUM, but it is a primitive rather than a single
hole.**

`loopback::select_discovery` (`loopback.rs:170-195`) prefers the discovery entry whose
`root` is the **deepest** ancestor of the child's cwd. So a single `Write` of a
*well-formed* `.cimp-discovery/<n>.json` naming a deeper root and a dead port makes any
child prefer a dead endpoint — and, because the entry parses, the resulting `ProxyMiss`
is `Transport` rather than `NoInstance`.

That gives an attacker a general **"force the fallback, and choose the reason it
reports"** primitive. M-8's fix is immune by construction (it discriminates on `--tab`
argv, never on the reason), but the primitive itself is untouched and also steers
`audit/mcp.rs` and every hook shim. `ProxyMiss`'s own doc comment — `Transport` is
*"the app is genuinely unreachable — the case the fallback was designed for"* — reads
as a safety property and is not one; `headless_refusal`'s doc records the correction
next to the decision that depends on it.

Related, not fixed: `HttpStatus`/`Unparseable` fall back at all, so a call the app
answered and **declined** is silently re-run headless, hiding the app's own verdict.
Under the `tab` rule the containment consequence is gone; the diagnostic dishonesty
remains.

---

### F-12 — a cloud backend is *advertised* `run_check`, which executes the project's configured commands

**Found by the M-2/F-10 fix run. Severity: HIGH. Predates this milestone. NEEDS A USER
DECISION — this is a policy call, not a missing copy of an existing gate.**

`run_check` appears in **neither** `LOCAL_DATA_TOOLS` (`schema.rs:2810-2817`) nor either
re-gate, and both routers dispatch it. Unlike F-10, it does not need a hallucinated
name: whenever the offload toggle is on and `checks` are configured, `run_check` is
**advertised** in the tool specs a cloud backend receives. It then executes the
project's configured build/test/lint commands and returns their output — which quotes
source — to a remote model.

So this is strictly wider than F-10, which is why it is numbered separately rather than
left inside F-10's entry: F-10 is now FIXED, and an unfixed wider hole must not inherit
that disposition.

It is not fixed here because the question it raises is a product decision, not a bug:
**does a cloud backend get the project's checks at all?** The three defensible answers
are (a) add `run_check` to `LOCAL_DATA_TOOLS`, denying it to cloud backends by default
and letting the user opt in — consistent with how `read_file`/`run_command` are treated,
and `run_check` is arguably closer to `run_command` than to anything on the allowed
side; (b) re-gate it behind its own opt-in the way `graph_*` and the audit tools are,
now cheap since `BackendGate` exists; (c) accept it deliberately and write down why.

Note (a) and (b) differ in an important way: `LOCAL_DATA_TOOLS` only removes it from the
*scope*, and the V8-02 default is applied by the Settings UI when a backend's cloud flag
is toggled — so it would not retroactively fix an existing configured backend. (b) is
enforced at call time and would. **Recommendation: (b), plus (a) for new backends.**

Note also that this is the third member of a family this review keeps meeting —
`run_check` also drove **M-8** (headless latch bypass) and the un-enveloped check output
behind **M-7's residual #1**. Any decision taken here should be checked against those
two rather than made in isolation.

---

### F-13 — `/latch/state` publishes `contaminated` and the OpenCode plugin never reads it

**Found by the M-15 fix run. Severity: MEDIUM. A published signal with no consumer —
global principle 3.**

`LatchRegistry::beacon` contaminates a `Local`-latched tab **without moving the latch**
(`beacon_row`'s own else-branch says so), and `/latch/state` publishes `contaminated`
alongside `latch` (`loopback.rs:5681`). The generated OpenCode plugin's gate reads only
`st.latch`. So a tab that is **contaminated but not EXTERNAL** admits every local tool,
and the bit that says so was on the wire the whole time.

This is not M-15's race — it is the steady state after it. M-15 closed the window during
which contamination has not yet been *reported*; F-13 is the case where it has been
reported, is published, and is ignored.

Deciding it needs care rather than a one-line read: `contaminated` is sticky for the
tab's life (H-2 removed the only reset, and the clear path is a deliberate human
authority action), so gating local tools on it would be much closer to a permanent
withdrawal than the latch is. That may well be right — it is what the bit means — but it
is a product decision, and it should be taken together with the contamination-clear path
built in session 3 rather than bolted onto the plugin.

### F-14 — H-1's "most hardened combination" rationale is overstated

**Found by the M-15 fix run. Severity: LOW (a doc/claim defect, not a code one).**

The comment at the epoch bump in `opencode_plugin_source` justifies the gate-cache
invalidation as covering the most hardened configuration. It does not:

- With **gate ON + beacon OFF**, the invalidation fires only for the tab's *own* web
  tools — and in that mode there is no report, so the re-query returns `open` anyway.
- A **proxied** `ddg` fetch that *does* move the latch app-side triggers **no**
  invalidation in the plugin at all; it relies purely on the 2 s TTL.

The mechanism is fine; the sentence describing it claims more than it delivers, which is
the same class of defect the O-1 documentation audit found seven of.

---

### F-15 — `GraphIndex::mem_add_note` still takes a raw `&str`

**Found by the M-20 fix run. Severity: MEDIUM.**

M-20's `NoteText` closes the **screen's** input, not the **store's**. `mem_add_note` is
still `pub` and takes a raw `&str`, so a future write path can store an unbounded,
unscreened note by calling it directly — the screen is only reached by the one caller
that happens to go through the `context_note` arm.

This is the same shape as the tripwire-gap section's existing note that
`mem_quarantined_notes` is a `pub` accessor returning tainted rows with no bound on its
callers: the guard is on the *path*, not on the *store*. Threading `NoteText` through the
storage signature would close it, at the cost of the index layer plus ~10 test call
sites. Deliberately not done inside M-20 — it is a wider refactor than the finding, and
it should be decided as one with the `mem_quarantined_notes` half rather than piecemeal.

### F-16 — two of the three `MemoryQuarantine` row producers write an empty `root`

**Found by the M-19 fix run. Severity: LOW, but it degrades a review surface.**

`record_secret_screen_flag` already wrote `root: String::new()`, and M-19's
`unattributed_write` matches it because `LatchRegistry::gate` has no scope to derive a
root from. Retention is unaffected — lanes key on `Screen`, not root — but a
**root-filtered Tool Activity view will not show these rows under any project**. So two
of the three producers of the row a user is meant to review are invisible to a
project-scoped reviewer, which is a consumer-quality gap of exactly the kind the
tripwire-gap analysis flags for M-21 and H-10.

### F-17 — stale test binaries wedge the linker (build reliability, not product)

**Hit three times during the M-19/M-20 run. Severity: LOW for the product, HIGH for
iteration speed.**

After several `cargo test` runs a `cimp-<hash>.exe` from a *completed* run survives and
the next link dies with `LNK1104: cannot open file …\deps\cimp-<hash>.exe`, needing a
manual `Stop-Process`. Plausibly the same child-process leak behind the known
`audit::runner::tests::{cancel_kills_child, timeout_kills_child_and_reports_timed_out}`
flakes — which spawn real children and assert kill timing. If so, one fix closes both,
and it would also make CI reruns less flaky.

### F-18 — every pointer to the V32 controls names a Settings section that does not exist

**Found live 2026-08-10 on `v0.51.0-rc.1`, by a user who went looking for the injection
settings and concluded the build did not have any. Severity: MEDIUM. Predates the
milestone in part (the section label does), but V32 is what made it load-bearing.**

The controls are all present, under `SectionId 'offload'` — labelled **"Offload task
tools"** (`SettingsApp.svelte:1075`) — as *Injection protection* (the master switch,
`:4414`), *Native web tools* (`:4573`), *Injection detection* (`:4610`) and *Detection
updates* (`:4783`), below *Backend pool* and *Limits*.

Three things compound, and the third is why the first two are not merely cosmetic:

1. **`TaintMenu.svelte:226` instructs the user to "Change it in Settings → Tools →
   Injection protection."** There is no Tools section. The seventeen-entry list is
   Appearance … Tabs, **Offload task tools**, MCP servers, Code Intelligence, Checks,
   Code Audit, LLM pricing, Workbench, Advanced, About. This string is reached exactly
   when a user has been told their protection is reduced — i.e. at the one moment the
   navigation has to work.
2. **The spec's own recipes 11b and 14 say "Settings → Tools → Detection."** A verifier
   following the live-verify section cannot find the surface those recipes test. This is
   the *Documentation truthfulness* class again: the third time this milestone's prose
   has described a surface that does not exist as written.
3. **The ⛨ chip promises a deep link it does not perform.** Four `latch.ts` strings
   (`:364`, `:374`, `:389`, `:391-392`) end "Click to open Settings", and
   `InjectionBadge.svelte:44` calls `openSettingsWindow()` with **no argument**, so the
   window opens on whatever section was last active — Appearance on a fresh profile. The
   mechanism exists and is used elsewhere: a `settings-deep-link` event with
   `{kind:'section', section}` is handled at `SettingsApp.svelte:1274`. Nothing needed
   building; the call site simply does not send it.

**Why this outranks the other UI-honesty MEDIUMs.** M-21 lies about one layer, M-22 about
one popover, M-24 blurs four states into one chip. F-18 makes the **entire** V32 control
surface — including the master switch, the detection health rows and the updater buttons
recipes 11/11b drive — unreachable for a user who follows the app's own instructions. A
containment control that cannot be found is off in the way that matters.

**Fix, cheapest first:** correct the strings to "Offload task tools"; have
`InjectionBadge` emit `settings-deep-link {kind:'section', section:'offload'}`; correct
recipes 11b and 14. The better fix renames the section — "Offload task tools" stopped
describing the block when V32 put every tab's latch under it — but a rename carries its
own drift surface and would want a sweep of the twenty-odd "Settings → X" strings across
`src/lib/`, several of which are already shorthand that matches no label ("Settings →
Offload", "Settings → Code graph").

**Process note, worth more than the finding.** This is the first V32 defect found by
running the build rather than reading it. It was invisible to every source-level pass
because each half is locally correct: the section renders, the string is well-formed, the
click opens Settings. Only the composition is wrong, and no test asserts that a
user-facing navigation string names a section that exists — which is itself the tripwire
gap to close (a test over the "Settings → …" literals against `SECTIONS` would be cheap
and would have caught all three).

### F-19 — the seeded price table has no `claude-opus-5` row, so session cost reads $0

**Found live 2026-08-10 on `v0.51.0-rc.2`. Severity: MEDIUM. Not a V32 defect** — it has
nothing to do with injection hardening — **filed here so the F-series stays in one
ledger** rather than starting a second one for the same testing run.

`default_llm_pricing()` (`src-tauri/src/settings/schema.rs:548-605`) seeds Claude Fable 5,
Opus 4.8, Opus 4.7, Opus 4.6, Sonnet 5, Sonnet 4.6 and Haiku 4.5, plus the Copilot rows.
There is no `claude-opus-5`. The Anthropic rows are the ones that carry a `model_prefix`,
which is what the Usage view's cost mode matches transcript model ids against (longest
prefix wins, `usageMath.ts:100`). A session on the current default model therefore matches
**no** row, falls back to manual-pick, and shows $0 until the user adds the row by hand —
which is what the live run did before anything else in this session worked.

**The part that makes it more than a one-line fix.** Adding the row corrects **fresh
installs only**. `read_global_llm_pricing` (`settings/persistence.rs:386-402`) returns the
seeded defaults *only when the global settings file does not exist*; a file that carries
`llm_pricing` — even as `[]` — keeps exactly what it has. That is deliberate and correct:
the table is user-editable, and a deep-merge against defaults would fight a user who has
edited a price or deleted a row they do not use. The consequence is that every install
that already exists stays wrong after a code fix lands, silently, because the symptom is a
$0 reading rather than an error.

**Fix.** Both halves, or it is not closed:

1. the seed row in `default_llm_pricing()`; and
2. a one-time, **append-only** migration that adds built-in rows whose `model_prefix` is
   absent from the stored table. It must never overwrite a price the user has edited, and
   it must not resurrect a row the user deliberately deleted — which argues for keying the
   migration on a stored "seeded through version N" marker rather than on row absence
   alone.

**Class, and why it is worth recording next to the V32 findings.** This is the same shape
as M-5 and M-21: a computed value that is *presented as authoritative* while being
silently unpopulated. A cost readout of $0 is not visibly "unknown" — it reads as free.
Global principle 3 applies: the signal has a consumer, so it has to be either right or
visibly absent. A model id that matches no pricing row should arguably render as "—" or
"unpriced" rather than $0.00, independently of whether the Opus 5 row is present, and that
would have surfaced this on day one instead of on a user's manual edit.
