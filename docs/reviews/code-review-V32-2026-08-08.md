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
| H-1 | C-1 open — `graph_struct_search`/`graph_repo_map` are TRUSTED source readers | **OPEN** | needs a decision amendment, not just a `TABLE` edit |
| H-2 | C-2 open — contamination clears on any newline-terminated byte | **FIXED** | `2c40136` — reset removed, not re-armed; amends decision 15 |
| H-3 | SSRF — `http:/host`, `http:host`, `http:\host` evade the screen | **OPEN** | |
| H-4 | Every shipped rule defeated by invisible whitespace | **FIXED** | `5920c92` — rules + normalizer + corpus; 20/50 evading → 0/53 |
| H-5 | Update channel inert by construction (302 vs redirect ban) | **FIXED** | `5920c92` — `raw.githubusercontent.com` branch + the missing guard |
| H-6 | Bundle authenticated by TLS + repo-write only | **DECLINED** | locked decision 28 — key would live inside the blast radius |
| H-7 | A cloned repo's `opencode.json` is executed configuration | **OPEN** | largely V33 (OS containment); record as known-open meanwhile |
| H-8 | `/audit/run`'s taint gate is opt-in by the caller | **FIXED** | `80375a9` — `tab` required + `consumer` narrowed; both halves |
| H-9 | Forensic activity rows are model-evictable | **OPEN** | |
| H-10 | A failed detection-status read renders the layer ARMED | **OPEN** | |
| F-2 | `^` anchors to byte 0, not line start — three patterns dead | **FIXED** | `5920c92` — folded into H-4 (same file, same class) |
| M-4 | A classifier that ran and failed is indistinguishable from clean | **FIXED** | `5920c92` — `Scored.failed` → `incomplete`, scored windows kept |
| N-8 | Unbounded `previous/classifier/<version>` retention | **FIXED** | `5920c92` — died with the classifier component |
| N-10 | Corpus-echo bundle passes the whole gauntlet | **PARTIAL** | `5920c92` — `coverage_floor` catches the curation case, not a hostile publisher |
| N-12 | 512 MiB aggregate-download exposure | **FIXED** | `5920c92` — died with the classifier component |
| O-1 | The H-1 race guard is `#[ignore]`d and CI never runs `--ignored` | **OPEN** | one extra step in `tests.yml` |
| — | Pre-existing 1-in-4 test flake on the global rule slot | **FIXED** | `5920c92` — guard made structural in `screen_blocking` |
| F-3 | The primary contamination path writes no forensic record at all | **OPEN** | raised by the fix run; blocks the clear path |
| F-4 | `is_configured_tab` is agent-agnostic — `(consumer, tab)` verified nowhere | **OPEN** | raised by the fix run |
| F-5 | `/graph_run` + `/mcp/call` share H-8's tab half | **OPEN** | raised by the fix run; a decision, not a bug |
| F-6 | H-2's decode proof degrades silently if the CLI drops `sessionId` | **OPEN** | raised by the fix run; wants a drift canary |

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

> **STATUS: OPEN.** The reviewer compiled the extractor against the app’s own `url 2.5.8` and executed it, so the table below is observed output rather than inference.

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

> **STATUS: OPEN.** Note that the guard test asserts the breach — it floods past the cap and never checks that the targeted row survived.

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

> **STATUS: OPEN.** Verified by hand, including that `latch.test.ts:227` pins the wrong behaviour as correct.

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
| M-2 | The A-1 fix **inverted a fail-closed default**: a native name missing from `TABLE` used to classify EXTERNAL and be refused/latched; now it "neither latches nor is refused" and falls through to dispatch. Load-bearing in both directions, with no test and no tripwire. | OPEN | `agent.rs:1258-1268`, `loopback.rs:1586-1589` |
| M-3 | Spotlighting is spawn-baked in practice (`fact_promotion_block` → `--append-system-prompt` / OpenCode instructions) but declared live: absent from `spawn_inject_sig`, no restart hint. Toggling it on mid-session leaves the running tab injecting **unenveloped pre-V32 memory into the system prompt**. Test pins the defect. | OPEN | `config.rs:1082-1089`, `injection.rs:330-337,1736` |
| M-4 | A classifier that runs and fails is indistinguishable from Clean, and a failing window **discards windows that already scored over threshold**. `Scored` has no field to express it. The spec names exactly two exclusions; the code implements three. | **FIXED** `5920c92` | `classifier.rs:391-399`, `mod.rs:404-409` |
| M-5 | At the worker, the "unscreened" notice is **false every time it fires**: `cap_result` truncates to 32 KB, unconditionally below both screening caps, so every byte the model sees was scanned. Trains the reader to discount a notice that is true at the proxy. | OPEN | `mod.rs:387-394`, `agent.rs:1910/1928` |
| M-6 | Audit findings enter model context with no envelope, no detection scan, and without contaminating the conversation — scanner-quoted text from `node_modules` is framed as authoritative project data, and `context_note` afterwards is *not* quarantined. `context_recall`, strictly tamer, *is* enveloped. | OPEN | `audit/mcp.rs:248-266`, `loopback.rs:2618-2621,1706-1708` |
| M-7 | Three ungated loopback routes reach local capability; `POST /context/post_edit` **executes the project's configured checks** for a caller-supplied `cwd`. None carries a `tab`; none appears in any route enumeration. | OPEN | `loopback.rs:3297-3328,2659,2768` |
| M-8 | `run_check` dispatches **above** the class gate on the headless path, so an EXTERNAL-latched tab that corrupts `.cimp-discovery` runs the project's build/test/lint while `ddg__*` stays live. | OPEN | `graph/mcp.rs:516-529` |
| M-9 | The `#46` outcome split covers the manifest fetch but not the **artifact** fetch: any asset 404/timeout is recorded as a bundle *rejection* (red card, `unreachable_streak` reset). The deploy note's own publish order makes this the likely steady state. | OPEN | `updater/mod.rs:1032-1039,1059` |
| M-10 | A crash *during* rollback deletes the files the rollback already restored — the journal has two phases and the rollback is an unrepresented third state. Permanent, uncarded, and `warn!`s "the previous version was restored". | OPEN | `updater/mod.rs:1423-1434,1365-1370` |
| M-11 | `restore_archived` swallows per-file failures; the caller then reports "the previous version was restored" verbatim, and `healthy` cannot see missing files. Silent permanent coverage loss with a reassuring message. | OPEN | `updater/mod.rs:1401-1413` |
| M-12 | Crash recovery only runs when the updater is enabled *and* something is due. A user who turns detection off after a crash strands a short/empty `rules.d` permanently — "never degrade to no rules" fails closed on an unrelated switch. | OPEN | `updater/mod.rs:1974-1999` |
| M-13 | An identifier collision between a shipped rule and a user rule freezes the update channel forever and blames the user's file — U-4's exact symptom, in the case the README tells users to expect. | OPEN | `updater/mod.rs:430-437` |
| M-14 | The updater's run lock is process-local; two instances sharing an exe directory race the swap and can destroy the old bundle with the journal pointing at an empty archive. | OPEN | `updater/mod.rs:490`, `store.rs:69` |
| M-15 | H-1's gate-cache fix narrows the race but does not close it: the epoch is bumped *before* the beacon POST and never after it resolves, so a query issued **during** the POST caches an `open` verdict for a full 2 s TTL. | OPEN | `config.rs:1758-1772,1650-1680` |
| M-16 | `read` is deliberately left unpinned to preserve OpenCode's `*.env` → "ask" carve-out — but last-match-wins applies to the *project* config too. A repo shipping `{"permission":{"read":"allow"}}` resolves `read * → allow` and `.env` is read with no prompt. Verified live. | OPEN | `config.rs:2038-2043` |
| M-17 | `/mcp/call` and worker error paths carry the remote MCP server's `error.message` verbatim and up to 300 chars of raw response body — unscreened, unwrapped, unbudgeted — while both call sites' comments assert these are cImp-composed strings. | OPEN | `mcp_host.rs:1282-1314`, `loopback.rs:3804` |
| M-18 | The SSRF widening reads CIDR notation and doc placeholders as fetch targets: `"RFC1918 (10.0.0.0/8)"` in a *search query* refuses the whole call with a security error. Compounds — each benign denial raises the power-of-two threshold that suppresses a later real one. | OPEN | `outbound.rs:386-415,660-684` |
| M-19 | Decision 21's "empty is not absent" reasoning is applied on the headless path but not the loopback path: a `PersistentWrite` with no resolvable tab identity is stored **unquarantined**, and `scoped_session` attributes it to another tab's session. | OPEN | `loopback.rs:1572-1584`, `graph/mcp.rs:1114-1125` |
| M-20 | `context_note` text is unbounded and the secret screen only sees the first 256 KiB — 256 KiB of filler then an AWS key stores Clean. `secrets.rs:120-128` asserts the opposite ("it cannot reach either bound"). | OPEN | `graph/mcp.rs:1107,1146` |
| M-21 | A worker-only detection override leaves the updater inert and the manual buttons lying: `updates_enabled` resolves at `Scope::App`, which deliberately excludes the worker row, so the UI says "detection is off" about a layer that is running. | OPEN | `updater/mod.rs:246-252`, `injection.rs:724-729` |
| M-22 | The override popover renders a click-time snapshot that never updates while open — a tab that becomes contaminated while the user reads it keeps saying "Not latched." | OPEN | `TabBar.svelte:68-102` |
| M-23 | Promote (unquarantines attacker-authored text into future sessions) is one unconfirmed click; Discard (which can only lose a note) is behind a modal. Polarity inverted vs. the latch UI, which gets it right. | OPEN | `CodeIntelligenceView.svelte:338-339` |
| M-24 | `Unscreened`, detector flags, `MemoryQuarantine` and `LatchOverride` collapse into one red chip; only denials are visually distinct. "We did not look at all of it" reads as "we blocked something" — the opposite. | OPEN | `ToolActivityView.svelte:356-375` |
| M-25 | The frontend branches on `rules.armed` where the backend publishes `rules.healthy` for exactly this question — 3 of 4 rule files failing renders full protection. `offload.ts:212` documents that `healthy` must be read, never restated. | OPEN | `latch.ts:259` |
| M-26 | `FILE_COMPACT_LINES` equals the sum of the per-kind caps (1000), so at saturation every single write triggers a full file read + atomic rewrite, and the accepted child-append race opens on every write instead of every ~1000. | OPEN | `activity.rs:58-84,472` |

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
| `/audit/run` gate | route is LOCAL-CAPABILITY gated | Tests exercise `gate()` and `tool_name_for` **directly**; deleting the `latches().gate(…)` block from `handle_audit_run` leaves the suite green. Same for the three lines in `audit/mcp.rs` that put `tab` in the body — deleting them produces H-8. |
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
| H-1 gate cache clobber race | MEDIUM | **PARTIAL** (M-15) — original interleaving closed, second one open |
| H-2 per-tab plugin flags | MEDIUM | **CLOSED**, with migration (not forward-only) |
| A-1 hallucinated name latches | MEDIUM | **CLOSED** functionally; introduced M-2 |

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

---

## Test & CI gaps

- **O-1 — the H-1 regression guard never runs in CI.**
  `the_gate_cache_survives_a_beacon_racing_an_in_flight_query` (`config.rs:4329`) is
  `#[ignore]`d because it needs `node` on PATH. CI runs `cargo test --locked --bin
  cimp` with no `--ignored` pass — **on a job that runs `npx vitest run` twelve lines
  earlier**, so node is unconditionally present. I ran it manually: it passes. One
  extra step in `tests.yml` fixes it.
- Three tests pin the defect rather than the invariant (see "The pattern").
- Deleting the gate call from `handle_audit_run` or `handle_run` leaves the suite
  green — decision 18's own enforcement has no test at its enforcement point, which
  is the shape #48 explicitly rewrote another test to avoid.
- No test binds `TABLE` to the dispatchable tool set (M-2's backstop).
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
