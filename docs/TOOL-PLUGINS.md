# Tool plugins — the capability contract

**Manifest version:** `manifest_version = 1`
**Spec revision:** 1 (V38 Phase G, 2026-08-19)

This is the plugin author's contract: what cImp promises around a tool it runs
on your behalf, what it hands the tool, what it does with the output, and what
it refuses. Design authority is
[MILESTONE-V38-tool-plugin-framework.md](MILESTONE-V38-tool-plugin-framework.md);
this document is the *interface*, and `src-tauri/src/plugins/` is the code half.

**The version is validated, not documented.** `plugins::manifest::MANIFEST_VERSION`
is checked equal to the number above by a test, and a file whose
`manifest_version` is anything else is refused at load with a message saying so
— a plugin written for a different cImp does not half-load.

**This document is drift-tested.** The blocks marked `<!-- drift:… -->` below are
parsed by `plugins::spec`'s tests and compared against the constants and tables
they describe. A character list, an environment name, a cap or an enum value that
changes in code without changing here fails `cargo test`. Keep those blocks in
their stated format; prose around them is free.

---

## 1. What a plugin is

A JSON file in `<cImp>/plugins/` describing one or more **tools**. It carries no
binaries and no code: the manifest is a declaration, and **you supply every
executable path** in Settings → Tool Plugins. That is the entire trust model, and
§ 6.6 states it plainly.

Each tool declares exactly one **capability kind**, which decides what cImp
guarantees around the spawn and which pipeline reads the output:

| kind | pipeline | output |
|---|---|---|
| `audit` | the `quality_audit` umbrella | SARIF findings |
| `security` | the `security_audit` umbrella | SARIF findings |
| `check` | `run_check` | diagnostics |
| `command` | `run_command` | raw text |

A tool also belongs to exactly one **category** — the management dimension, what
the user toggles as a group. Category carries **zero contract weight**: nothing
in this document changes because of which category presents a tool (§ 7).

---

## 2. The manifest

One file per plugin, `<cImp>/plugins/<anything>.json`. Unknown fields are an
error, not a shrug: a key cImp does not understand is refused by name, because a
typo that silently does nothing surfaces weeks later as a behaviour difference.

### 2.1 Identity and namespacing

`name` and `version` are both mandatory; together they are the plugin's identity.

* `name` is `[A-Za-z0-9_-]+`; `version` is the same set plus `.` and `+` — and
  deliberately **not** `@` or `/`, which would make the tool key ambiguous.
* A tool's global id is **`name@version/tool-id`**. Two versions of one plugin
  coexist without either shadowing the other.
* **Exact duplicate** (same name AND version in two files) ⇒ **neither loads**,
  and the conflict names both file paths. Not "first wins": which file won would
  depend on directory read order, so the same pair would behave differently on
  another machine.
* Same name, different version ⇒ both load, and settings disambiguates them.
* The `cimp-` prefix is **reserved** for cImp's own plugins and refused in a
  scanned file. It is in use: the fourteen Code Audit scanners are
  `cimp-audit@1` (§ 10).
* A plugin's tool ids must be unique within it, and every tool must belong to
  exactly one category — a category toggle is a group operation, so a tool in two
  categories would have two conflicting group states and a tool in none would be
  unreachable in the only UI that controls it.
* A plugin declaring **no tools** is refused. A well-formed no-op would sit in
  the settings list forever explaining nothing.

The two charsets are not prose. Every row below is fed to the real validator by
a drift test, so a sentence in this section cannot outlive the rule it
describes. Format: `field:verdict:value`, with `<empty>` for the empty string.

<!-- drift:identity-charset -->
```text
# `name` and the id charset (tool ids and category ids share it)
name:accept:acme
name:accept:acme-tools
name:accept:acme_tools
name:accept:ACME9
name:refuse:acme.tools
name:refuse:acme+tools
name:refuse:acme@1
name:refuse:acme/tools
name:refuse:<empty>
# `version` — the id charset plus `.` and `+`, and never `@` or `/`
version:accept:1
version:accept:1.0.0
version:accept:1.0.0-rc.1
version:accept:1.0.0+build_7
version:refuse:1.0.0@x
version:refuse:1/0
version:refuse:1,0
version:refuse:<empty>
```

### 2.2 Discovery and rescan

The `plugins/` directory beside the cImp executable, and **only** there — the
same exe-adjacent arrangement `themes/` and `palettes/` use. There is no
per-project plugins folder, on purpose: a project directory is writable by
anything running in it, and a manifest is an argv template plus a grant request.

Every `*.json` in that directory is read at startup, non-recursively, in sorted
order, and again on the **Rescan** action in Settings. Rejections are **loud**:
each one becomes a settings error state *and* a `plugin` Events row naming the
file and the reason. Nothing is ever skipped silently.

The registry is read at **invocation** time, so enabling a tool, changing its
path, or rescanning takes effect on the next call — nothing is baked at spawn.

### 2.3 Fields

Plugin level: `manifest_version`, `name`, `version`, `label?`, `description?`,
`categories[{id, label, tools[]}]`, `tools[]`.

Tool level, every kind:

| field | meaning |
|---|---|
| `id`, `label`, `kind` | identity and capability kind |
| `description?` | one line saying what the tool is for, shown beside the label |
| `enabled_by_default` | whether the tool is on before the user touches it (default `true`) |
| `runtime` | which sandbox runtime profile applies (§ 6.2) |
| `sandbox` | what cImp does when it cannot confine this tool (§ 6.1) |
| `extra_grants[]` | absolute paths to grant beyond the profile (§ 6.3) |
| `parameters_allowed` | whether the settings pane offers a free-form "extra CLI parameters" field (`audit`, `security`, `check`) |

Kind-specific fields, refused when they appear on the wrong kind:

| field | kinds | meaning |
|---|---|---|
| `argv[]` | `audit`, `security` | the argv template (§ 3.2) |
| `transport` | `audit`, `security` | `stdout` or `report_file` |
| `findings_exit_codes[]` | `audit`, `security` | non-zero exits that still mean "ran fine, here are findings" |
| `applicability{extensions,markers}` | `audit`, `security`, `check` | project-shape gate (§ 3.6); both empty = always applicable |
| `provider{server,tool}` | `audit`, `security` | tier 2 (§ 4.5): an MCP server answers instead of a spawned binary; mutually exclusive with every spawn field |
| `cmd` | `check` | the command line template (§ 4.2) |
| `cwd` | `check` | run here instead of the project root; relative, confined |
| `report_file` | `check` | parse this file after the run instead of stdout; relative, confined |
| `pattern` | `check` | the `regex-custom` parser's pattern |
| `parser` | `audit`, `security`, `check` | see § 5.2 and § 4.2 |
| `variables[{name,label,default?}]` | `audit`, `security`, `check` | the settings fields cImp renders, and the only names `{var:…}` may reference |
| `timeout_secs` | `audit`, `security`, `check` | per-tool wall-clock budget |
| `env[[k,v]]` | `audit`, `security`, `check` | environment forced onto the child |

A `command`-kind tool is **identity only**: the path and the enable come from
user state, and the arguments come from the caller. It therefore refuses
`timeout_secs`, `env`, `variables`, `parameters_allowed` and `applicability` at
load, by the same cross-check that refuses `argv` on a `check`. Nothing on that
kind would read them — `run_command` runs every tool under one fixed budget
because it is advertised to a model as a short read-only probe, it composes its
child environment from cImp's allowlist plus the applicable `CommandPolicy`, it
has no template to substitute a variable into, it takes its arguments from the
caller rather than from stored state, and it resolves ONE named tool on demand
rather than fanning out over a population a project-shape gate could filter.
Refusing is reversible and consuming is not: a manifest written against a field
that silently did nothing would change behaviour the day it started working.
(`parameters_allowed` is a plain boolean, so only `true` can be refused — an
explicit `false` says exactly what this rule wants and cannot be told apart from
an absent field.)

### 2.4 Limits

Every one of these is refused at load (or applied at run) rather than trusted.

<!-- drift:caps -->
```text
# key = value; each is pinned against the constant it describes.
manifest_version              = 1
manifest_max_bytes            = 1048576
identity_max_chars            = 100
timeout_min_secs              = 1
timeout_max_secs              = 86400
check_timeout_floor_secs      = 10
check_timeout_default_secs    = 120
audit_timeout_default_secs    = 600
audit_output_max_bytes        = 16777216
audit_findings_report_cap     = 300
audit_report_max_bytes        = 65536
audit_event_findings_per_tool = 500
run_command_timeout_secs      = 120
run_command_output_max_bytes  = 32768
```

`manifest_max_bytes` is checked before the file is read as well as after: the
plugins directory is writable by anything running as you, and the startup scan
reads every `.json` in it, so one enormous file must be a rejected plugin rather
than an out-of-memory launch. `timeout_secs = 0` is refused rather than
interpreted — it would mean both "no limit" and "kill it at once" depending on
who read it.

### 2.5 Fields cImp stamps, and a file may never claim

Format: `field:verdict:a valid value for it`. `never` = refused in every file,
embedded or scanned; `builtin-only` = accepted in cImp's own embedded manifests
and refused in a scanned one. Both verdicts are exercised against the real
validator by a drift test, so "reserved" here means refused rather than
discouraged.

<!-- drift:reserved-fields -->
```text
builtin:never:true
ingest:builtin-only:"grandfathered"
command:builtin-only:"acme-scan"
project_local_bin:builtin-only:"acme"
dir_argv:builtin-only:["--dir","{root}"]
```

`builtin` is **not a manifest field, in any file**. Whether a plugin is built in
is stamped by the loader; the security-relevant gates (the reserved prefix, the
findings-parser rule, the security floor) key off that stamp and never off a name
string. A file carrying it is refused with a message that says so, rather than
with serde's generic "unknown field" — which would read as a typo instead of as
the provenance forgery it is.

The other four are **built-in only**, and each of them relaxes a rule this
document states for everyone else:

* `ingest: "grandfathered"` relaxes the output gate to the semantics the
  fourteen built-in scanners were measured against (§ 4.1). A user plugin may
  never select it, because the strict gate is what stops a blank artifact
  reading as a clean scan.
* `command` is a bare command NAME cImp resolves through `ebin` then `PATH` when
  no path is configured — the one exception to "no automatic PATH resolution"
  (§ 5.2). It is a name, never a path.
* `project_local_bin` prefers a `node_modules/.bin` shim over a global install
  when no path is configured (the built-in `eslint` and `knip`).
* `dir_argv` is a second argv template used when the scan root is not a git
  repository (the built-in `gitleaks`, whose `dir` form differs from its `git`
  one). A user plugin that wants the distinction makes it inside the wrapper it
  already points cImp at.

---

## 3. Input surface

### 3.1 What a plugin never receives

Nothing about the session: no prompt, no conversation, no tab identity, no
harness token, no cImp settings file. The trust unit is **manifest + path +
argv** — everything cImp gives a plugin's process is described below and nowhere
else.

### 3.2 Argv and command templates

Three tokens, in `argv[]` (audit/security) and `cmd` (check):

| token | renders to | available in |
|---|---|---|
| `{root}` | the project root cImp is scanning/running in | every template |
| `{report}` | the report file cImp will read afterwards | `audit`/`security` **iff** `transport = report_file` |
| `{var:NAME}` | the value of a declared variable | every template |
| `{{` | a literal `{` | every template |

* `{report}` and `transport` must agree in both directions: using the token under
  `stdout` transport is refused (there is no path to substitute), and declaring
  `report_file` without ever using the token is refused too (cImp would read a
  file the tool was never told to write).
* `{report}` is **forbidden for `check`** — a check names its output file in
  `report_file` instead.
* `{var:NAME}` may only name a variable the same tool declares.
* An undeclared or unset variable renders **empty**. Empty is the honest
  rendering of a value that does not exist.

**Substitution is single-pass, and that is a security property, not an
optimization.** Variable values and CLI parameters can be set per project, in
`.cimp/config.json`, which lives inside the project root — a directory every
sandboxed child can write. So a value is attacker-reachable input. It is copied
into the output and **never looked at again**: a value of `{report}` lands in
argv as those eight literal characters, and `{var:a}` can never expand into
`{var:b}`.

The **program is never in the template.** You supply the path; cImp spawns that
file. For `audit`/`security` it is argv[0] and the template is the rest. For
`check`, whose `cmd` is a whole command line, the manifest's first token is a
*placeholder* that cImp replaces with your configured binary — which is why a
`cmd` that does not begin with a plain program name (a `NAME=value` prefix, a
pipeline, a redirection) is refused **at load**, not at the first call: a plugin
is authored once and read on every machine, so it must not load on one and fail
mid-session on the next.

### 3.3 Parameters

When `parameters_allowed` is set, the settings pane offers a free-form extra-CLI
field. Those parameters are appended **verbatim, in order, after** the rendered
template — as separate argv elements for `audit`/`security`, and as
space-separated (and, if they contain a space, double-quoted) words for `check`.
They are never substituted into the middle of anything, and they are screened
first (§ 6.5).

Stored parameters on a tool whose manifest does **not** set `parameters_allowed`
are dropped: state that outlives a manifest change must not become argv on a tool
whose author never opted into an appendable command line.

### 3.4 Environment

The child's environment is composed in a fixed order, and the order is the
contract:

1. **the base** — cImp's own environment for a plain (unsandboxed) `audit` or
   `check` spawn; the allowlisted table in § 6.4 for a sandboxed one, and for
   every `run_command` spawn regardless of sandbox state;
2. **the manifest's `env` pairs**, forced onto the child;
3. **the sandbox's own redirections** (`TEMP`/`TMP`/`HOME`/`USERPROFILE` pointed
   at the mapped drive) — these **always win**, because a child that writes its
   scratch outside the sandbox's one writable place is a child that gets denied.

So a manifest can set what a tool needs and cannot unset what the boundary
needs. Env keys are shape-checked at load (non-empty, no `=`, no NUL, no
whitespace); the *allowlist* that decides what a sandboxed child sees is § 6.4's.

### 3.5 Working directory and file confinement

| kind | cwd |
|---|---|
| `audit`, `security` | the project root (the scan root, which is the launch directory) |
| `check` | the project root, or the manifest's `cwd` confined strictly beneath it |
| `command` | the first allowed root of the calling session |

`cwd` and `report_file` are relative and confined: absolute paths (in the
platform-agnostic sense — a POSIX-rooted path is absolute on Windows too, for
this purpose) and any `..` component are refused at load, and the confinement is
re-checked against the real root at spawn.

A `report_file` transport's `{report}` path is chosen by cImp, in a directory
cImp owns, and granted read-write to the sandboxed child for the duration.

### 3.6 `applicability` — the project-shape gate

`applicability` decides whether a tool exists **for this project at all**. Both
lists empty (the default) = no gate. Otherwise the tool applies when ANY listed
extension OR ANY listed marker was seen — an OR across both lists, never an AND.

It is honoured on `audit`, `security` **and** `check` kinds, by one function
(`Census::admits`), so a gate cannot mean one thing under an umbrella and
another under `run_check`:

| kind | what the gate removes |
|---|---|
| `audit`, `security` | the tool is not spawned in the fan-out; the report shows it as `skipped-not-applicable` rather than dropping it silently |
| `check` | the check is not advertised to `run_check` and cannot be dispatched — the advertised name list and the runnable one are the same list |
| `command` | refused at load: `run_command` resolves one named tool on demand and has no population to filter |

**`extensions`** are lowercase and dot-less (`java`, `cs`, `py`) and match any
file seen in the walk. **`markers`** are not filenames — they are tokens from a
CLOSED vocabulary cImp owns, so a manifest can only ask about project shapes the
census was built to see. A marker outside this list never matches (it is not an
error; it simply never fires):

<!-- drift:markers -->
```text
go.mod
Cargo.toml
package.json
*.sln
*.csproj
eslint.config
.eslintrc
pom.xml
build.gradle
```

`*.sln` and `*.csproj` are families (any file with that extension), `eslint.config`
covers the flat-config spellings and `.eslintrc` its dotted variants, and
`build.gradle` covers `build.gradle.kts` — one project shape, one token.

The census walk is bounded and `.gitignore`-aware, and it is **cached for up to
60 seconds per root**. Truncation and staleness can only ever HIDE a tool, never
invent one: adding a `pom.xml` to a project shows up on the next walk rather than
instantly, and nothing watches the filesystem to pulse a `tools/list_changed` for
it.

The gate is not a substitute for the enable and the path. A tool with no
configured binary is inert whatever its `applicability` says, and that
complementary mechanism — **path-unset-is-the-gate** — is what disambiguates a
tool whose project shape has no marker token (a Python project, say). Both are
available; the gate is automatic and the path is deliberate.

---

## 4. Output contracts

### 4.1 `audit` and `security` — SARIF

**Purpose.** Pick these when your tool produces *findings about code*: a scanner,
a linter with a security ruleset, a dependency auditor. `security` joins the
`security_audit` fan-out, `audit` joins `quality_audit`. Nothing else differs.

**The contract is SARIF, and it is enforced.** A user plugin's `parser` must be
`sarif` — refused at load, not merely documented. If your tool has no `--sarif`
mode, mediate it inside the plugin (a wrapper that transforms its native output
is a normal plugin shape; a wrapper is a binary *you* point cImp at, so it costs
no new trust).

Output is read from stdout, or from the report file for the `report_file`
transport. Then the **ingest gate**:

| what the tool wrote | verdict |
|---|---|
| nothing (empty, whitespace, BOM only) | **tool ERROR** |
| not JSON | **tool ERROR** |
| JSON, but no `runs` array | **tool ERROR** |
| `{"runs": []}` | **clean scan**, zero findings |
| `{"runs": [ … ]}` | findings ingested |

**One exception, and it is cImp's own.** The fourteen built-in scanners predate
this contract and their behaviour was measured against the real binaries rather
than designed: a clean `gitleaks` run writes **no report at all**, and `cppcheck`
exits 0 whether or not it found anything. Their embedded manifests therefore
carry `"ingest": "grandfathered"` (§ 2.5), which keeps the pre-V38 semantics —
whatever the tool wrote is what it meant, including nothing. A user plugin may
not select it, so the table above is the whole of the contract for anything you
install.

The middle three rows are the point. *Empty is not absent*: a tool that printed
nothing said nothing, and reading zero findings out of it would report a clean
project on the strength of a tool that failed. `runs: []` is a SARIF log saying
"I ran and found nothing", which is exactly what a clean scan looks like — so
that, and only that, is the clean answer.

**Attribution is stamped from the registry entry that was spawned.** The
`runs[].tool.driver.name` inside the output is deliberately ignored: a name
inside output is a claim by the thing being audited.

**Other failure modes.**

* *Schema-valid but semantically wrong* — cImp cannot detect a lie about severity
  or location, and does not pretend to. What it does guarantee is that the
  findings are attributed to the tool that produced them, that they pass the
  same screening every other finding does (§ 4.4), and that they are additive:
  the built-in security tools are **always** in the `security_audit` fan-out, so
  a plugin can add findings but can never make the umbrella report less than it
  did before. That invariant exists specifically against a plugin that attacks by
  under-reporting.
* *Hang* — the tool is killed at its timeout (§ 2.4) and reported `failed`; a
  spawn helper that does not settle within the timeout plus a fixed slack mints a
  `wedged` row rather than waiting forever. Other tools in the same scan are
  unaffected. A scan is also cancellable mid-flight.
* *Flood* — each stream is capped at `audit_output_max_bytes`; the excess is
  drained and discarded so the child never blocks on a full pipe.
* *Non-zero exit* — an exit code in `findings_exit_codes` means "ran fine, here
  are findings". Any other non-zero exit is a failure, and its stderr is what the
  chip reports.
* *Not applicable* — `applicability` gates on the project census (any listed
  extension **or** any listed marker file). A gated-out tool is not run and is not
  a failure.
* *Cannot be prepared* — a tool that belongs to the umbrella but cannot run (an
  unresolvable parser, a `sandbox: required` refusal, a path that has gone
  missing) is reported as a **failed chip carrying the reason**, never dropped:
  a tool the user enabled and pointed at a binary must not vanish from a report
  in silence.

### 4.2 `check` — diagnostics

**Purpose.** Pick this for the compile/lint/test loop: a command line that
produces diagnostics a model can act on, in a format `run_check` already parses.

**Invocation.** `run_check{name}`. A plugin check's advertised `name` is its
manifest-local tool id, unless something already answers to that name — in which
case it becomes the full `name@version/tool-id`, and after that the key plus a
counter. **A configured check is never shadowed**: the project's own `checks`
array is laid down first and a plugin renames itself around it, because a plugin
that could claim the name `cargo` could make `run_check{name:"cargo"}` run
anything at all. Plugin checks are included in the advertised name enum and in
its schema fingerprint, and since V38 Phase F that fingerprint also **notifies**:
a settings save or a plugin Rescan that moves the effective check set emits one
debounced `tools/list_changed`, so a live session picks the new roster up (same
session for OpenCode, next turn for Claude Code) instead of showing the old enum
until it restarts. One pulse per action, and none at all when the set did not
actually move — the applicability gate of § 3.6 is part of "the set", but a
filesystem change that flips a gate is not watched for: it is seen on the next
advertisement, within the census's 60-second cache.

`run_check` hands the rendered command line to the platform shell (`cmd.exe /S /C`
on Windows, `sh -c` elsewhere), which is what a check has always been. That is
also why § 6.5's screen exists.

**Output.** Whatever `parser` names, from the `run_check` diagnostics vocabulary
(`sarif`, `cargo-json`, `tsc`, `eslint-json`, `pytest`, `go`, `junit-xml`,
`regex-custom`, …). The `parser` word is disambiguated **by kind**: on a `check`
it selects a diagnostics decoder; on an `audit`/`security` tool the same word
selects a findings decoder. Naming a findings-only parser on a check (or a
diagnostics-only one on an audit tool) is an error, never a silent default —
decoding output with the wrong parser yields zero diagnostics, which reads
exactly like a clean run.

Set `report_file` to parse a file the tool wrote instead of its stdout; a missing
report file is an explicit error diagnostic, not an empty pass.

**Failure modes.** Timeout kills the process (and, on Unix, its whole process
group) and reports partial output. Output is capped. A rendering failure —
an unresolvable parser, a missing `cmd`, a refused value — makes the check
**advertised but broken**: naming it returns the real reason. It is not dropped
from the enum, because a capability the user configured must not vanish without
explanation.

### 4.3 `command` — raw output

**Purpose.** Pick this for an ad-hoc dev binary a model should be able to invoke
directly: `git`, `svn`, a project's own CLI.

**Invocation.** `run_command{command, args}`. A registered `command`-kind tool
that is enabled **and** has a path becomes both the permission and the
resolution: the registry is consulted first, and a match runs *that exact file*
rather than resolving the name through `PATH`. A miss falls through to the
project's `command_allowlist` unchanged.

Everything else applies to both populations: the bare-name guard (the caller
names a program, never a path), the per-program `CommandPolicy` argument rules
(a policy is a statement about *arguments*, which does not stop being true
because the binary came from a plugin), the fixed timeout and the output cap
(§ 2.4), and the sandbox.

**Output.** Raw, truncated at the cap with a notice, prefixed with the exit code.
There is no parser and no `parser` field.

**Failure modes.** A configured path that does not exist is reported as a
configuration problem naming the tool and the path, not as an opaque spawn error.
A tool that is enabled but has **no** path is inert — cImp never picks a binary
for a plugin — and every refusal says so by name.

### 4.5 Tier 2 — an MCP server answers instead (`provider`)

**Tier 1 is the default, and this is the escape hatch.** A manifest that names an
`argv` is a complete definition: cImp spawns the tool, confines it, caps its
output and reads its SARIF, and the only long-running process on the machine is
the one you already installed. A `provider` tool hands all of that to a server
the *user* administers. The milestone's standing bias is to keep the
standing-MCP population **small** — supervision load, notification churn and
per-process trust are real costs, and "writing an argv looks like work" is not a
reason to pay them. Reach for tier 2 when the tool is genuinely service-shaped:
it already exists as an MCP server, or it holds state no per-scan process could.

**Declaring one.** On an `audit` or `security` tool only, and instead of the
spawn fields — never alongside them:

```json
{
  "id": "acme-cloud",
  "label": "Acme Cloud Scan",
  "kind": "security",
  "provider": { "server": "acme-mcp", "tool": "scan_repository" }
}
```

`server` is the server's id in cImp's MCP registry (its **name**, which is what
V37 keys activation and categories by — renaming the server is a new identity and
breaks this reference). `tool` is the tool's name on that server; cImp composes
the `<server>__<tool>` routing name itself.

**Mutually exclusive with the spawn vocabulary, by refusal.** `argv`,
`transport`, `findings_exit_codes`, `env`, `variables`, `parameters_allowed`, a
non-default `runtime` or `sandbox`, and any `extra_grants` are all refused at
load on a provider tool. Nothing is spawned, so each of them would describe a
child process that never exists — and a sandbox declaration in particular would
be a boundary the author believes in and cImp never prepares. `kind` stays the
contract dimension: a `security` provider tool joins the `security_audit`
fan-out exactly like a spawned one.

**Invocation.** The tool joins its umbrella's fan-out as one more member. Instead
of a spawn, cImp issues **one `tools/call`** to the named server, through the
same host path every proxied MCP call takes. That path is the enforcement point:
a disabled server — or a server whose every category is off — refuses the call
with the same words any other caller gets, the outbound URL screen runs, and the
`mcp` Events lane records the call with the server, its category, and `audit` as
the consumer. Health state is *not* consulted as a gate: dispatch is the truth,
and a server marked unhealthy that nevertheless answers must not be refused by
cImp.

**Input surface — nothing is passed.** The call carries an empty argument object.
The server scans what it is configured to scan; cImp does not send the project
root, because a provider has no guarantee of sharing this machine's filesystem
and a path it cannot read is worse than no path. If your server needs to be told
*what* to look at, configure that on the server.

**Output contract — the same SARIF, through the same gate.** The result's text
content is read exactly as a spawned tool's stdout is: the full SARIF envelope
check (§ 4.1), then the shared parser, then attribution to the **registry key**
rather than to whatever `runs[].tool.driver.name` claims. `parser` is forced to
`sarif` and `ingest` is refused, so the two relaxations that exist for cImp's own
fourteen scanners cannot be borrowed here. A blank, non-JSON or non-SARIF answer
is a tool **error**, never a clean scan.

**Failure modes.** Every one of them is a failed chip carrying the reason, and
none of them is a pass:

* *Server not configured, or its tools not reachable* — the host answers that
  nothing offers the tool.
* *Server or category disabled* — the refusal names the level that did it.
* *The server returned an error* — its own message, bounded.
* *Timeout* — the tool's `timeout_secs` (else the umbrella's) bounds the call.
* *Scan cancelled* — the call is abandoned cleanly. There is no child to kill, so
  a request already in flight completes on the server's side and its answer is
  discarded.

**Consumption path and screening.** Findings enter the report through the same
boundary as tier 1 and pass the same delivery screening before a model sees
them. The server itself is classified **EXTERNAL** like every other configured
MCP server, so its tool descriptions are screened at connect time and a flagged
one is withheld from every surface.

**Security posture — what you are trusting.** There is no sandbox here, because
nothing runs on this machine. What you extend by enabling a provider tool is
trust in **the server you configured**: its answer becomes findings in a report a
model reads, and cImp guarantees only that the answer is a well-formed SARIF log
that said something. That is the same shape as tier 1's statement — *enabling a
tool means trusting the executable you pointed it at* — with the executable
replaced by a service, and it is strictly the larger ask: a binary you chose runs
confined, for one scan; a server you configured runs continuously, answers every
call, and is administered by whoever administers it.

**Reachability.** cImp's own subsystems reach an MCP server through the
per-server "offload worker" grant — that flag has always meant *cImp itself may
use this server* — so a provider tool's server needs that box ticked, in addition
to being enabled. There is deliberately no fourth per-server checkbox for the
audit fan-out: a new grant dimension is a settings decision, not something an
audit feature adds on its way past.

### 4.4 What reaches a model

Findings and check output pass the delivery boundary's existing screening before
a model sees them: injection detection over the raw text, spotlighting, and the
report envelope, exactly as for any other tool output. A plugin adds no new
delivery path.

The report a model reads is capped at `audit_findings_report_cap` findings and
`audit_report_max_bytes` bytes, whichever binds first, with a line saying it was
truncated; the UI's per-tool event payload is capped separately at
`audit_event_findings_per_tool` and the full set is pulled on demand.

**Tool descriptions never name an underlying binary**, before or after plugins,
so installing or enabling one changes what the fan-out runs and emits no
`tools/list_changed` churn at all. The model-facing schema is unchanged by
plugins, with one deliberate exception: `run_check`'s `name` enum, which has
always been project-dynamic.

---

## 5. Configuration and scoping

### 5.1 Where each value lives

| value | scope | stored |
|---|---|---|
| plugin enabled | global | settings |
| tool enabled | global | settings |
| tool **path** | machine-global, optionally **per project** | the global settings file: one machine-wide map keyed by tool, plus a per-project map keyed by project root |
| `timeout_secs` override | global | settings |
| variable values | global, **project-overridable** | settings; the project layer in `.cimp/config.json` |
| CLI parameters | global, **project-overridable** | settings; the project layer in `.cimp/config.json` |

**The project overlay never widens the boundary.** `.cimp/config.json` lives
inside the project root, which the sandbox grants in full — so anything running
in the project, a compromised model included, can write it. Therefore:

> Enables, binary paths, timeouts and `extra_grants` are **never** read from the
> overlay, on any leg. Variable values and CLI parameters **do** ride the overlay
> on **every** leg — stripped identically by one function, treated as untrusted
> at render time, and screened before they can reach a shell.

A banned field appearing in the overlay is ignored with a loud `plugin` Events
row, and a write-through keeps project-scoped path edits saving to the
machine-global per-project map, so the *capability* (two projects, two paths)
survives while the storage location does not sit inside the sandbox.

### 5.2 Resolution rules

* **Enabled** = the plugin's switch AND the tool's. Disabling a plugin does not
  clear its tools' own flags, so re-enabling restores the selection you had.
* **Path** = this project's entry, else the machine-wide entry, else unset.
  A tool with no path is **inert** — visible, unrunnable. Installation is not
  activation; there is no automatic `PATH` resolution, because cImp never picks a
  binary on your behalf.
  * **The one exception is cImp's own tools.** A plugin whose provenance the
    loader stamped `builtin` may declare a bare `command` name, and with no path
    configured cImp resolves that name through the `ebin` drop-in folder and then
    your `PATH` — which is exactly how the Code Audit scanners have resolved
    since V23. The rule above protects you from cImp guessing a binary for a
    definition a stranger wrote; it was never an argument for making fourteen
    shipped scanners stop working. The gate is the loader's provenance stamp, not
    the `cimp-` name.
* **Runnable** = enabled AND path set. Two separate questions, kept separate, so
  "why is my enabled tool not running?" has an answer.
* **Variables** = declared defaults, overlaid by your values, **for declared
  names only**. A stored value whose name the manifest no longer declares is kept
  (a plugin mid-upgrade) but never substituted.

---

## 6. Security posture

One posture, resolved the same way at all three seams (the audit fan-out,
`run_check`, `run_command`), from three manifest fields — plus one machine-wide
switch that is not a manifest field at all.

### 6.0 Network, and the tool that needs egress

`allow_network` is a **single cImp-wide setting** (Settings → Sandbox), off by
default. It is deliberately not a manifest field: per-host scoping is not
something either OS engine can express today, so the honest granularity is
all-or-nothing for the whole app, and a plugin asking for "just this one host"
would be a promise cImp cannot keep.

What "off" means for a plugin's child process:

* **Windows (AppContainer).** The container is created without the
  `internetClient` capability, so no outbound socket succeeds — DNS included.
  With the switch on, that capability is granted whole; it was measured on a
  Public-profile NIC to open the **LAN** as well as the internet, which is why
  the switch is one bit rather than a list.
* **Linux (Landlock).** On an ABI 4+ kernel, TCP `bind()` and `connect()` are
  denied. **UDP is not restricted at all**, so a direct-socket DNS query still
  leaves the machine, and on a kernel below ABI 4 there is no network
  confinement whatsoever. Both facts are stated in the sandbox lane's posture
  line rather than papered over.
* **Either platform, sandbox switched off.** No boundary, therefore no network
  restriction — the tool has whatever access you do.

The consequence for a plugin author is concrete. A tool that **needs** egress —
a scanner fetching a registry ruleset, an auditor querying an advisory database,
anything that resolves a name — fails inside the boundary on a default install,
and it fails looking like a tool error rather than like a policy decision:
an empty report, or an exit code with nothing on either stream.

> **If your tool needs the network, declare `sandbox: unsupported`.** That runs
> it outside the boundary as a stated, visible choice, with a row saying so and
> the ask shown beside the switch that enables it. Do **not** declare `optional`
> and hope: `optional` means "run degraded when no boundary is available", so on
> a machine where one *is* available the tool runs inside it — which is exactly
> the case that breaks. And `required` on a network tool means it never runs at
> all.

**Taxonomy.** cImp classifies every tool name a dispatcher can serve
(`offload::toolclass::TABLE`) and **defaults an unknown name to EXTERNAL**, the
most restrictive class. Plugins add no model-visible name at all, so nothing a
plugin does moves a class: the seams a plugin reaches through
(`security_audit`, `quality_audit`, `run_check`, `run_command`) keep the
`LocalCapability` rows they were reviewed into before plugins existed. A plugin
widens what those tools *run*; it never changes the class their output is
handled under, nor the injection screening it passes on the way to a model
(§ 4.4).

### 6.1 `sandbox` — what happens when cImp cannot confine the tool

<!-- drift:sandbox -->
```text
required
optional
unsupported
```

| value | behaviour |
|---|---|
| `required` (default) | The tool is **not run** when the OS boundary cannot be provided — **including when the sandbox is switched off in cImp's settings**. The refusal is the answer, loudly, in the sandbox Events lane and in whatever surface the seam reports through. |
| `optional` | Run degraded, with a visible row saying the boundary was not available. |
| `unsupported` | Run **outside** the boundary as an informed choice, with a visible row. Nothing is prepared for it: no ACE is stamped and no drive is mapped for a tool that declared it can use neither. |

`required` is the default because an author who has not thought about
confinement should get the safe answer. It overriding the global switch is
deliberate: a manifest saying "this tool must be confined" is a statement about
the tool, and a global preference is not an argument against it.

### 6.2 `runtime` — which profile's grants apply

<!-- drift:runtime -->
```text
none
python
node
java
dotnet
go
rust
auto
```

A **request that cImp stamp grants from a table cImp owns**, never a path. That
is what keeps the field safe: the worst a lying manifest achieves is a grant the
user can see named at enable time. A free-form runtime path would make the
manifest a grant-widening primitive.

* `none` is the positive statement *"single static binary — its own directory is
  the whole grant"*.
* `auto` (the default) keeps cImp's inference from the resolved program.
* Every other value selects a cImp-owned runtime profile, which supplies that
  runtime's install-tree grants and the environment pointers it needs (a
  redirected cache here, a re-asserted `HOME` there).

**Declaration ⇄ inference cross-check.** Inference keeps running even when you
declare, as a canary. cImp runs with **your declaration** — inference cannot know
a runtime it has never met — and records the disagreement as a `runtime-mismatch`
row rather than silently trusting either side. `auto` is the only value exempt,
because `auto` *is* the inference; an explicit `none` is checked like any other
declaration, since a stale `none` is the most consequential kind to have wrong
(the tool runs with no runtime grants at all). If a tool fails to start inside
the boundary, that row is the first thing to read.

Note that a project-local tool (an `eslint` or `knip` resolving out of
`node_modules/.bin`) is runtime-dependent yet often needs `none`: its payload
already lives inside the project root the sandbox grants.

### 6.3 `extra_grants` — the escape hatch

Absolute paths a tool needs beyond its runtime profile: a rules tree, a shared
model, a runtime image no profile covers.

* **Read + execute only.** The two places a tool legitimately writes are already
  granted (the project root, and cImp's report directory for a `report_file`
  tool), so a write ACE here could only widen the boundary past the field's
  purpose.
* **Shown to you at enable time as a permission**, the phone-app pattern.
* **Global scope only** — never overlay-settable (§ 5.1).
* Shape-checked at load: absolute (platform-agnostically) and free of `..`.
* **Screened at spawn** against the refusal rules below. A refused path is
  dropped and every other grant still applies: a bad grant must not brick a tool,
  and must not silently widen the boundary either.

<!-- drift:grant-refusals -->
```text
# trailing path components that are never granted, compared lowercased
.ssh
.aws
.gnupg
.config/gh
microsoft/credentials
microsoft/protect
microsoft/vault
```

…plus four structural rules that come first and stop the wholesale cases: a
relative path, a volume/filesystem root, a user-profile root or an ancestor of
one, and the Windows install directory or anything under it.

**One honest wrinkle.** A refusal is only *reported* when a boundary is actually
being prepared. With the sandbox off, or on a tool declaring `unsupported`, the
child can read that directory freely, and telling you a grant was withheld would
be worse than saying nothing — the run's honest row is the unsandboxed one the
seam already mints. A residual case remains: when the sandbox is on but turns out
to be unavailable on this machine, screening happens before that is known, so a
grant-refusal row can appear beside the skip row for a run that had no boundary.
Both rows are true; the pair is a little redundant.

### 6.4 The environment a sandboxed child sees

The ceiling — names absent from cImp's own environment are simply not set, and
nothing outside this list is passed on. It is the base for every sandboxed
`audit`/`check` spawn and for every `run_command` spawn.

<!-- drift:child-env -->
```text
# process plumbing
PATH
PATHEXT
COMSPEC
SystemRoot
SystemDrive
windir
TEMP
TMP
TMPDIR
NUMBER_OF_PROCESSORS
PROCESSOR_ARCHITECTURE
OS
# per-user state directories
HOME
USERPROFILE
HOMEDRIVE
HOMEPATH
APPDATA
LOCALAPPDATA
ProgramData
ProgramFiles
ProgramFiles(x86)
XDG_CACHE_HOME
XDG_CONFIG_HOME
XDG_DATA_HOME
# toolchain state pointers
CARGO_HOME
RUSTUP_HOME
RUSTUP_TOOLCHAIN
npm_config_cache
npm_config_prefix
NPM_CONFIG_CACHE
NPM_CONFIG_PREFIX
# locale and time
LANG
LC_ALL
LC_CTYPE
TZ
```

### 6.5 The value screen — a contract, not a nicety

A `check`'s command line goes through the platform shell. Variable values and
CLI parameters can be set **per project**, in a file the project can write. So
substituting them is substituting attacker-reachable text into shell source, and
it is the one place a declarative manifest could become arbitrary code execution.

cImp **refuses the run** rather than quoting around it. `cmd.exe`'s quoting rules
are not `CommandLineToArgvW`'s, `^` escapes survive quotes, `%VAR%` expands
inside them, and getting that right for every value shape on two shells is
exactly the kind of "we handled it" that ships a hole. A value that needs `&` or
`$` in a linter's ruleset name is not a use case.

Refused in a variable value or a CLI parameter, on the `check` seam:

<!-- drift:shell-unsafe -->
```text
& | ; < > ( ) ` $ " ' ^ % !
```

…and every control character.

**Deliberately not refused**, because each is legitimate and none is shell
syntax that could escape the argument:

<!-- drift:shell-allowed -->
```text
* ? ~ { } #
```

(Spaces, backslashes and every other ordinary character are allowed too; the
block lists only the ones a reviewer might expect to see refused. `#` is last
because a leading `#` is this block format's comment marker.)

* `#` starts a comment in `sh`, which can only ever **truncate** the rest of the
  command line — it drops work, it does not add any. Refusing a `#` would break
  every colour, fragment and anchor a real value contains.
* `*` `?` `~` are globbing. A glob changes which files a tool reads, inside a root
  the tool can already read.
* `{` `}` are brace expansion in some shells, and are ordinary characters in
  paths and in JSON-ish values.

An `audit`/`security` tool's values are **not** screened this way and do not need
to be: they become elements of an argv vector that is spawned directly, with no
interpreter to re-read them. The screen exists exactly where a shell does.

### 6.6 The trust statement

> **Enabling a plugin's tool means trusting the executable YOU pointed it at.**
> cImp guarantees the definition is well-formed, that the boundary and the caps
> described here are applied, and that the output is screened before a model
> reads it. It does not vouch for the tool. There is no approval flow, no hash
> pinning and no signing, because a plugin carries no binaries — the trust
> decision is yours, made when you fill in the path, and it is the same act as
> today's per-tool path overrides.

---

## 7. Categories carry no contract

A category groups tools for the user: "Java" holding a compiler, a build tool
and two scanners; "Source control" holding `git` and `svn`. It gives them one
toggle. That is all it does. Nothing in §§ 3–6 changes because of which category
presents a tool, no pipeline branches on it, and a tool of any kind may live in
any category.

---

## 8. Scope, and what plugins deliberately do not do

* **Plugin checks are on-demand.** They are reachable from `run_check` and from
  the audit roster, and nowhere else: they are never auto-run, never run against
  a worktree, and are not exercised by the Settings "Test" button. Adding a
  capability to a project must not silently add work to every save.
* **The harness chooses among tools; cImp does not arbitrate.** Configuration governs
  *availability* through two complementary mechanisms, and neither is cImp
  forming an opinion about your project:
  * the **enable and the path** — a tool with no configured binary is inert, so
    configuring maven and leaving gradle unset is a complete answer on its own;
  * the **applicability gate** (§ 3.6) — `pom.xml` → maven, `build.gradle` →
    gradle, on `audit`/`security`/`check` kinds alike, over the closed marker
    vocabulary § 3.6 lists. `command`-kind tools have no gate, by refusal.

  Where a project is genuinely ambiguous — both markers present, both tools
  configured — both are offered and the harness picks, which is exactly where
  judgement belongs.
* **The security floor stays.** Plugins add to the built-in security tools and
  can never replace or displace them.
* **No shadowing.** A plugin id can never claim a built-in id; the two namespaces
  are structurally disjoint (a plugin key always contains `@` and `/`).
* **Two `command`-kind tools with the same id, in different plugins, collide.**
  `run_command` matches by tool id, so the first in registry order (plugin key,
  then manifest order) wins and the second is unreachable under that name. Give a
  `command`-kind tool the id of the program it registers, and do not register a
  program another installed plugin already registers.
* **A malformed entry in the machine-global settings container** (a path map keyed
  by something that is not a tool key, say) is dropped with a `tracing` warning
  and nothing user-visible. Manifests get a settings error state and an Events
  row; a hand-edited settings file does not.
* **No per-project plugin definitions** and no plugin-supplied binaries. An
  MCP-backed provider (§ 4.5) is the one place a plugin's work happens somewhere
  cImp does not control, and it is `audit`/`security` only — there is no
  provider-backed `check` or `command`.

---

## 9. Drift tests

`plugins::spec` parses each `<!-- drift:… -->` block above and asserts it against
the code:

| block | pinned against |
|---|---|
| `drift:caps` | `MANIFEST_VERSION`, `MAX_MANIFEST_BYTES`, `MAX_NAME_CHARS`, `MAX_TIMEOUT_SECS`, the check timeout floor and default, the audit timeout default and output cap, `MAX_FINDINGS`, `MAX_RESULT_BYTES`, `EVENT_FINDINGS_PER_TOOL_CAP`, `run_command`'s `TIMEOUT` and `MAX_OUTPUT_BYTES` |
| `drift:kinds` | `ToolKind`'s serde wire names |
| `drift:sandbox` | `SandboxReq`'s serde wire names |
| `drift:transports` | `Transport`'s serde wire names |
| `drift:runtime` | `RuntimeReq`'s serde wire names, and that each named profile exists in `sandbox::RUNTIME_PROFILES` |
| `drift:legacy-parsers` | `LegacyAuditParser`'s wire names, and that a user plugin cannot select one |
| `drift:grant-refusals` | `sandbox::GRANT_REFUSAL_RULES` |
| `drift:child-env` | `sandbox::child_env::CHILD_ENV` |
| `drift:shell-unsafe` | `checks::plugin::SHELL_UNSAFE` |
| `drift:shell-allowed` | that none of those characters is in `SHELL_UNSAFE` |
| `drift:reserved-fields` | that a scanned manifest carrying any of them is refused, and that the built-in-only ones load when the loader stamped `builtin` |
| `drift:identity-charset` | `manifest::valid_id` / `valid_version`, both verdicts on both fields |
| `drift:builtin-roster` | the tool keys of cImp's own embedded manifests |
| `drift:markers` | `audit::census::MARKERS`, the closed applicability vocabulary |
| `drift:packaging` | that `build.rs` and `.github/workflows/release.yml` both still ship the `plugins` directory |

Format rules for those blocks: an HTML comment `<!-- drift:NAME -->` immediately
followed by a fenced block; lines beginning with `#` are comments; blank lines are
ignored; everything else is whitespace-separated values. Parsing normalizes line
endings, so a CRLF checkout reads the same as an LF one.

---

## 10. cImp's own plugins

The framework has exactly one user: **cImp**. The fourteen Code Audit scanners
are not a second, privileged tier — they are embedded manifests parsed by the
same validator, joined by the same registry, spawned through the same runner and
configured in the same settings pane as anything you drop in the folder. That is
deliberate, and it is the only honest test of whether the contract above is
sufficient: a framework whose own author needed a private door has not been
shown to work.

What they get that a scanned file does not is exactly the four built-in-only
fields of § 2.5, each one documented, refused on the scanned path, and gated on
the loader's provenance stamp rather than on the `cimp-` name.

**These are settings keys.** Per-tool enables, timeouts, variable values and
binary paths are stored under `cimp-audit@1/<tool-id>`, and the schema v33 → v34
migration writes exactly these strings when it moves the pre-V38
`code_audit.tools` array into the container. The `1` is the identity of the
shipped set, not the cImp release it came in — bumping it would orphan every
one of those keys.

<!-- drift:builtin-roster -->
```text
cimp-audit@1/osv-scanner
cimp-audit@1/gitleaks
cimp-audit@1/semgrep
cimp-audit@1/oxlint
cimp-audit@1/golangci-lint
cimp-audit@1/ruff
cimp-audit@1/cppcheck
cimp-audit@1/typos
cimp-audit@1/eslint
cimp-audit@1/pmd
cimp-audit@1/knip
cimp-audit@1/cargo-machete
cimp-audit@1/dotnet-analyzers
cimp-audit@1/semgrep-quality
```

Two of them declare `enabled_by_default: false` — `dotnet-analyzers` runs a real
build (it restores packages and writes `obj/` and `bin/`) and `semgrep-quality`
fetches its ruleset over the network. Nobody should get either by accident.

A tool's WIRE id — what a finding is attributed to, what a chip is keyed by,
what the report a model reads prints — is the bare tool id (`osv-scanner`), not
the namespaced key. The two namespaces stay disjoint because a plugin key always
contains `@` and `/` and a built-in id never does (§ 8).

<!-- drift:kinds -->
```text
audit
security
check
command
```

<!-- drift:transports -->
```text
stdout
report_file
```

<!-- drift:legacy-parsers -->
```text
typos-jsonl
knip-json
machete-text
```
