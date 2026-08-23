//! **OpenCode's generated session config** — `OPENCODE_CONFIG_CONTENT`, the
//! managed instructions file, and the pinned permission block (V35 Phase K:
//! moved verbatim from `tabs/config.rs`).
//!
//! The env-var analogue of Claude's `--settings` overlay
//! ([`crate::harness::claude::overlay`]): one JSON document, computed at tab
//! spawn from one settings snapshot, carrying this harness's `mcp` map, its
//! `instructions` pointer, the `local-llama` provider block and — V32 Phase D
//! locked decision 8 — the explicitly restated `agent.build.permission` values
//! that an upstream default change must not be able to move silently.
//!
//! It moved for the same reason the Claude overlay did: every key in it is a
//! name OpenCode chose, so an upstream rename should be a diff in
//! `harness/opencode/`, not somewhere inside a 7000-line file about tab
//! launching. `tabs::config::compose_ai_env` still decides *that* an OpenCode
//! tab gets this env var; what it says is decided here.
//!
//! **Spawn-baked**, like everything else at this layer — see
//! `tabs::config::spawn_inject_sig` for the restart hint.

use std::path::Path;

use crate::settings::injection::NativeWebMode as NativeWebVisibility;
use crate::error::{AppError, AppResult};
use crate::offload::server;
use crate::settings::{AiToolTabConfig, LocalProviderBlock, Settings};
use crate::tabs::config::{
    compose_capability_guidance, consumer_hygiene_for,
    native_web_for,
};

/// Deterministic path of the managed OpenCode instructions file for `cfg`.
/// One file per tab id (the TTS toggle is per-tab) under a managed dir next to
/// the exe (the portable root, like the offload discovery file), falling back
/// to the OS temp dir. Pure — computing the path never touches the filesystem,
/// so `build_opencode_config` stays test-safe; the actual write happens on the
/// real launch path (`build_ai_tool_spec`).
pub(crate) fn opencode_instructions_path(cfg: &AiToolTabConfig) -> std::path::PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir)
        .join("opencode-instructions");
    // Tab ids are kebab-case reserved ids or `ai-<uuid>` duplicates — safe as a
    // filename stem.
    dir.join(format!("{}.md", cfg.id))
}

/// Write the managed OpenCode instructions file for `cfg` (idempotent
/// overwrite at each launch so it tracks live settings). Best-effort: a write
/// failure just means OpenCode launches without the guidance addendum, exactly
/// like a Claude tab with TTS injection disabled. Removes a stale file when no
/// guidance applies so a since-disabled toggle doesn't leave dead instructions.
pub(crate) fn write_opencode_instructions(cfg: &AiToolTabConfig, settings: &Settings) {
    let path = opencode_instructions_path(cfg);
    let text = compose_capability_guidance(cfg, settings);
    if text.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let _ = std::fs::write(&path, text);
}

/// Best-effort: add `.opencode/` to `<project>/.git/info/exclude` so the
/// generated plugin (and OpenCode's own `.opencode/.gitignore`) don't show up in
/// `git status`. No-op when there's no `.git` dir or the line is already present.
pub(crate) fn git_exclude_opencode(working_dir: &Path) {
    let info_dir = working_dir.join(".git").join("info");
    if !info_dir.is_dir() {
        return; // not a git repo (or a worktree/submodule shape we won't touch)
    }
    let exclude = info_dir.join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".opencode/") {
        return;
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".opencode/\n");
    let _ = std::fs::write(&exclude, next);
}

/// V32 Phase D — the pinned OpenCode `agent.build.permission` values (locked
/// decision 8). Each is the EFFECTIVE OpenCode 1.18.13 default for that tool,
/// restated explicitly so an upstream default change cannot move it silently;
/// see the long rationale at the injection site in [`build_opencode_config`]
/// (including which stricter values a user may deliberately flip to).
pub(crate) const OPENCODE_PINNED_BASH: &str = "allow";
pub(crate) const OPENCODE_PINNED_EDIT: &str = "allow";
pub(crate) const OPENCODE_PINNED_WEBFETCH: &str = "allow";
pub(crate) const OPENCODE_PINNED_WEBSEARCH: &str = "allow";

/// #48 (M-16) — the pinned `read` rule: the wildcard plus OpenCode 1.18.13's own
/// secret-file carve-out, restated verbatim.
///
/// Phase D deliberately left `read` UNPINNED so that a secret-file pattern
/// upstream adds later would still reach the tab — drift in the safe direction,
/// which pinning must not block. What that reasoning missed is who *else* writes
/// this key: rules are evaluated **last-match-wins across the merged config**,
/// and cImp runs OpenCode additively, so a cloned repo shipping
/// `{"permission": {"read": "allow"}}` appends `read *: allow` after the base
/// carve-out and `.env` is read with **no prompt**. Verified live. An unpinned
/// key is not "open to upstream", it is "open to whoever wrote the repo".
///
/// This restates today's EFFECTIVE behaviour and nothing else — the same
/// discipline as the four values above, so a working tab is undisturbed.
///
/// **The cost Phase D named is real and accepted**, in both directions: a
/// secret-file pattern upstream adds later will not reach the `build` agent
/// until it is added here, and a user's own *tightening* of `read` in their
/// project config is now overridden too (identical to the cost already accepted
/// for `bash`/`edit`/`webfetch`/`websearch`; the escape hatch is switching
/// consumer hygiene off for that tab). Widening the set beyond upstream's four
/// patterns (`*.pem`, `id_*`, `.npmrc`, …) is a BEHAVIOUR change for a tab the
/// user works in daily and is therefore a separate, deliberate decision — the
/// same line Phase D drew for `webfetch: "ask"`.
///
/// **`OPENCODE_DISABLE_PROJECT_CONFIG` is not this fix and does not replace it.**
/// It closes H-7's other three vectors (`mcp`, `plugin`, `instructions`) which
/// this pin does not touch, and V19 §A.3 scopes it default-on only for hardened
/// tabs — a population that excludes the default install where M-16 was
/// verified. See H-7.
///
/// ⚠ **INSERTION ORDER IS LOAD-BEARING** — the only pinned value for which that
/// is true. Last-match-wins means the `"*"` wildcard must come FIRST and
/// `"*.env.example"` (which `"*.env.*"` also matches) must come LAST.
/// `serde_json` preserves insertion order in this build; that is a transitive
/// feature (`preserve_order` → `indexmap`), not a declared one, so
/// `the_pinned_read_rule_keeps_the_env_carve_out_in_wildcard_first_order`
/// asserts the SERIALIZED string rather than the parsed value.
pub(crate) const OPENCODE_PINNED_READ_ANY: &str = "allow";
pub(crate) const OPENCODE_PINNED_READ_ENV: &str = "ask";
pub(crate) const OPENCODE_PINNED_READ_ENV_EXAMPLE: &str = "allow";

/// [`OPENCODE_PINNED_READ_ANY`] & co. assembled in their load-bearing order.
pub(crate) fn opencode_pinned_read() -> serde_json::Value {
    serde_json::json!({
        "*": OPENCODE_PINNED_READ_ANY,
        "*.env": OPENCODE_PINNED_READ_ENV,
        "*.env.*": OPENCODE_PINNED_READ_ENV,
        "*.env.example": OPENCODE_PINNED_READ_ENV_EXAMPLE,
    })
}

/// V32 Phase F (locked decision 14): the value the two web permissions take in
/// `deny` mode. OpenCode's permission vocabulary is `allow`/`ask`/`deny`, and
/// the block is already pinned per-agent — so the mode is a one-value swap on
/// the two keys Phase D deliberately left at their upstream defaults.
const OPENCODE_DENIED: &str = "deny";

/// The env var OpenCode reads its whole session config from — the harness's
/// name for the channel `tabs::config::compose_ai_env` writes it on.
pub(crate) const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";

// ── 2026-08-17: the local server's Basic auth (capability `opencode.route.noauth`) ──
//
// OpenCode's TUI hosts an HTTP server on the `--port` cImp launches it with, and
// until today cImp relied on that server accepting UNAUTHENTICATED loopback
// calls — a Tier-D dependency whose second edge was an unauthenticated local
// surface on which `POST /session/:id/message` (without `noReply`) starts a real
// agent turn. Upstream has a documented answer, live-spiked on the installed
// binary today: set a non-empty `OPENCODE_SERVER_PASSWORD` on the child and
// every route, `GET /event` included, requires HTTP Basic auth (unauth ⇒ 401,
// Basic ⇒ 200/SSE). So cImp sets one per spawn and authenticates its own tap and
// push, which is the D→B move locked decision 2 says outranks new features.
//
// Four properties of upstream's implementation shape everything below, and each
// one is a way to get this subtly wrong:
//
//  1. **The password is snapshotted at module load in the child**, so it must be
//     on the child's environment AT SPAWN. Setting it later does nothing.
//  2. **An EMPTY password silently disables auth entirely** — the dangerous
//     default, and exactly the "empty is not absent" failure of global principle
//     5. [`new_server_password`] can only return a non-empty string, and
//     [`server_basic_auth`] refuses to build a header for an empty one.
//  3. **Credentials go in the `Authorization` header, never the `auth_token`
//     query param.** A present-but-wrong query param WINS over a correct header
//     and 401s, so cImp sends the header and nothing else — which also keeps the
//     secret out of URLs (and therefore out of logs).
//  4. **There is no unauthenticated health route.** Only three static asset
//     paths bypass auth, so any readiness poll must either carry credentials or
//     be a bare TCP connect.
//
// First-party clients are unaffected: the TUI, `opencode run` and the plugin's
// own SDK client all read the same env and authenticate themselves, which is why
// setting a password does not break the tab cImp launched.

/// The env var whose non-empty value turns on HTTP Basic auth for OpenCode's
/// local server.
pub(crate) const SERVER_PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";

/// The env var carrying the Basic-auth username. Upstream defaults it to
/// [`SERVER_USERNAME`]; cImp sets it explicitly anyway — belt and braces, and
/// the same thing OpenCode's own desktop app does — so a future change to that
/// default cannot silently invalidate the header cImp sends.
pub(crate) const SERVER_USERNAME_ENV: &str = "OPENCODE_SERVER_USERNAME";

/// The Basic-auth username cImp sets and authenticates with.
pub(crate) const SERVER_USERNAME: &str = "opencode";

/// A fresh per-spawn server password: 32 hex characters of UUIDv4 entropy.
///
/// The same source and shape as the loopback bearer token
/// (`offload::loopback::make_token`, two UUIDs) — deliberately, so there is one
/// answer in this tree to "where does a per-launch secret come from" and no new
/// RNG dependency. Half the length because this one is a password on a loopback
/// socket rather than the app's own bearer, and 128 bits of a CSPRNG is not the
/// weak link in a threat model where any process running as this user can read
/// the plugin file.
///
/// **Never persisted and never in argv**: it is regenerated at every tab spawn,
/// lives in the child's environment and in the reader's in-memory
/// `Authorization` header, and is not Settings-derived — so, exactly like the
/// Claude hook token, it owes no `tabs::config::spawn_inject_sig` entry (that
/// signature exists to nag about values a *user* changed).
pub(crate) fn new_server_password() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The two env entries an OpenCode child must be spawned with for `password` to
/// take effect, as `(name, value)` pairs.
///
/// Returned as a list rather than written into a map by this function so the
/// caller (`tabs::config::compose_ai_env`) keeps its one composition order and
/// its per-tab override semantics — and so the two variable NAMES, which are
/// OpenCode's, are spelled once, here, beside the document that explains them.
/// That is the same rule [`CONFIG_ENV`] follows, and the layering scan enforces
/// it: both names are `Dep::ConfigKey`s on `opencode.route.noauth`, so they are
/// needles no production file outside `harness/` may contain.
pub(crate) fn server_auth_env(password: &str) -> [(String, String); 2] {
    [
        (SERVER_PASSWORD_ENV.to_string(), password.to_string()),
        (SERVER_USERNAME_ENV.to_string(), SERVER_USERNAME.to_string()),
    ]
}

/// The `Authorization` header value cImp's own calls to a tab's OpenCode server
/// must carry — `Basic base64("opencode:<password>")`.
///
/// `None` for an empty password, which is the whole point: an empty
/// `OPENCODE_SERVER_PASSWORD` disables auth upstream, so "no credential" and "a
/// credential that is the empty string" must not produce the same header. A
/// caller that gets `None` sends no header, which is correct for an
/// unauthenticated server and 401s (visibly, into the `VisibleOff` degradation)
/// against an authenticated one.
pub(crate) fn server_basic_auth(password: &str) -> Option<String> {
    use base64::prelude::*;
    if password.is_empty() {
        return None;
    }
    Some(format!(
        "Basic {}",
        BASE64_STANDARD.encode(format!("{SERVER_USERNAME}:{password}"))
    ))
}

/// The credential for a child that will be spawned with `env` — read back OUT of
/// the composed environment rather than remembered from generation.
///
/// The child's effective password is whatever ends up in its environment, and a
/// per-tab `env` entry deliberately wins over everything cImp synthesizes
/// (`compose_ai_env`'s last step). Deriving the reader's header from the same map
/// is what keeps the tap authenticating with the password the server will
/// actually be using — including the case where a user sets their own. `None`
/// when the variable is absent or empty, i.e. when that child's server will be
/// unauthenticated.
pub(crate) fn server_auth_from_env(
    env: &std::collections::HashMap<String, String>,
) -> Option<String> {
    env.get(SERVER_PASSWORD_ENV)
        .map(String::as_str)
        .and_then(server_basic_auth)
}

/// V19: synthesize OpenCode's session-scoped config — the JSON document that
/// `OPENCODE_CONFIG_CONTENT` carries (the env-var analog of Claude's
/// `--mcp-config` / `--settings` / `--append-system-prompt`):
///
/// - `$schema` marker.
/// - `subagent_depth: 2` (D-8) — pins nested-subagent behavior across the
///   OpenCode 1.18.2 default change (see the injection site).
/// - `agent.build.permission` (V32 Phase D, locked decision 8) — pins the
///   default primary agent's `bash`/`edit`/`read`/`webfetch`/`websearch` policy
///   at OpenCode 1.18.13's effective defaults, so neither an upstream default
///   shift nor a **cloned repo's own `opencode.json`** can move it silently
///   (#48, M-16: last-match-wins applies to the project config too). Per-agent,
///   not top-level, so the restrictive native agents
///   (plan/explore/compaction/title/summary) keep their own denials — the
///   injection site spells out why.
/// - `mcp.cimp-offload` → `cimp --offload-mcp --consumer opencode`, injected
///   whenever offload, the graph, or an OpenCode-exposed MCP server is in play
///   (mirrors the Claude `--mcp-config` gate in `build_pre_args`).
/// - V26 `mcp.cimp-code-audit` → `cimp --code-audit-mcp --consumer opencode`,
///   injected when Code Audit is enabled AND opted in for the OpenCode consumer
///   (`code_audit.expose_opencode`). OpenCode caches `tools/list` at connect, so
///   flipping the flag needs a tab restart to take effect (known caveat).
/// - `instructions` → the managed guidance file (TTS + offload + graph), when
///   any guidance applies. The file content is written on the launch path; here
///   we only reference its (deterministic) path.
///
/// - `provider.local-llama` + a default `model` → injected only when the user
///   has registered the local `llama-server` as an OpenCode provider (Offload
///   settings "Add to OpenCode", or auto-sync). Otherwise omitted, leaving
///   provider/model selection entirely to OpenCode's own global config.
///
/// Additive by default — cimp does not set `OPENCODE_DISABLE_PROJECT_CONFIG`, so
/// a user's project config still merges underneath. That is **not benign** and
/// H-7 records it. The keys this function writes win; **three it does not write
/// merge in and take effect**, and each buys a different thing:
///
/// * **`mcp`** — an arbitrary local command, spawned by the harness at launch
///   with the tab's environment. Nothing in cImp's containment model sees it:
///   it is not a cImp spawn seam, so it is outside the V33 C1 ledger, the
///   `run_command` minimal environment (C2) and the Job Object (C3) alike.
/// * **`plugin`** — a hostile ES module loaded into the harness's own process.
///   It can do anything the agent can, and it used to get the whole V32
///   in-process control surface for one line: assigning `globalThis.fetch`
///   disarmed the Phase F beacon and the Phase H gate together while cImp still
///   reported both ON. V33 C6 narrows that specific move — the generated plugin
///   now binds `fetch` at module evaluation ([`opencode_plugin_source`]) — but
///   the bound is LOAD ORDER, and in-process code was never a boundary. The
///   honest statement is that this key hands a repo code in the agent's process.
/// * **`instructions`** — resolved BEFORE cImp's Phase D contract paragraph, so
///   a repo gets to speak to the model ahead of the untrusted-content contract
///   that is supposed to frame everything the model then reads.
///
/// The pinned `agent.build.permission` block (including `read`, #48 M-16)
/// answers the last-match-wins half of this and nothing above. Pure: no
/// filesystem I/O.
///
/// V28: `tab` is the launching tab's id, appended to the `cimp-offload` child's
/// argv as `--tab <id>` (the OpenCode-side mirror of the Claude `--mcp-config`
/// injection) so `context_*` calls resolve to THIS tab's session.
pub(crate) fn build_opencode_config(
    cfg: &AiToolTabConfig,
    settings: &Settings,
    tab: &str,
) -> serde_json::Value {
    let mut config = serde_json::Map::new();
    config.insert(
        "$schema".to_string(),
        serde_json::Value::String("https://opencode.ai/config.json".to_string()),
    );

    // D-8 (maintenance 2026-08-04): pin `subagent_depth`. OpenCode 1.18.2
    // introduced this key with a default of **1**, which lets a primary agent
    // launch subagents but blocks those subagents from launching their own —
    // a silent behavior change for any workflow that nested before. cImp's
    // installed OpenCode predates it, so upgrading would quietly break nesting
    // unless the injected config states an intent. `2` restores one level of
    // nesting (the pre-1.18.2 shape) without going unbounded.
    //
    // Deliberately a CONSTANT, not a setting: `spawn_inject_sig` only needs an
    // entry for Settings-derived spawn injections, and a constant can never
    // differ between a running tab and a fresh one.
    //
    // Key verified 2026-08-04 against both https://opencode.ai/docs/config/
    // ("You can control how deeply subagents can invoke other subagents using
    // the `subagent_depth` option… The default is 1") and the live schema at
    // https://opencode.ai/config.json (top-level integer, minimum 0,
    // "Maximum subagent nesting depth. Defaults to 1, which prevents subagents
    // from launching subagents."). Additive, so a user's own project config
    // still merges underneath and can override it.
    config.insert("subagent_depth".to_string(), serde_json::Value::from(2u64));

    // V32 Phase D (locked decision 8): pin the tool-permission policy instead
    // of inheriting upstream defaults, which have shifted across versions
    // before (the 1.18.9 SDK v2 revert and the 1.18.2 `subagent_depth`
    // introduction above are the precedents). The milestone locks that the
    // values are PINNED, not that behaviour changes — so what goes in is
    // exactly what OpenCode 1.18.13 does today, giving drift-immunity without
    // disturbing a working tab.
    //
    // ── What upstream actually defaults to (verified 2026-08-06) ───────────
    // Source of truth: the bundled default ruleset inside the installed
    // `opencode.exe` 1.18.13 (`Permission.fromConfig({...})` in the agent
    // service), corroborated by https://opencode.ai/docs/permissions/ ("Most
    // permissions default to `allow`"; `doom_loop` and `external_directory`
    // default to `ask`). The built-in base ruleset is:
    //     { "*": "allow", doom_loop: "ask",
    //       external_directory: { "*": "ask", <cwd>/<tmp>/<config dirs>: "allow" },
    //       question: "deny", plan_enter: "deny", plan_exit: "deny",
    //       read: { "*": "allow", "*.env": "ask", "*.env.*": "ask",
    //               "*.env.example": "allow" } }
    // so `bash`, `edit`, `webfetch` and `websearch` all resolve to "allow"
    // through the `"*"` wildcard. Rules are evaluated last-match-wins.
    //
    // ── Why this is under `agent.build`, not top-level `permission` ────────
    // A top-level `permission` block is merged LAST into EVERY native agent's
    // ruleset (`merge(base, <agent overrides>, <user config>)`), so it would
    // override, not pin:
    //   * `plan` sets `edit: {"*": "deny"}` — "Plan mode. Disallows all edit
    //     tools." A top-level `edit: "allow"` re-enables editing in plan mode.
    //   * `explore`, `compaction`, `title` and `summary` set `"*": "deny"`;
    //     a top-level pin hands each of them back bash/edit/webfetch — the
    //     exact "model-derived text gains execution" shape V32 exists to stop.
    // `agent.<name>.permission` is merged onto that one agent only
    // (`e.permission = merge(e.permission, fromConfig(s.permission))`), so
    // pinning `build` — the default primary agent an OpenCode tab starts in —
    // freezes the working agent's policy and nothing else.
    //
    // ── Stricter alternative, deliberately NOT taken ───────────────────────
    // `"webfetch": "ask"` (and/or `"bash": "ask"`) turns the two capabilities
    // an injected page most wants — network egress and command execution —
    // into per-call user confirmations. That is a real hardening step and the
    // natural follow-up once the V32 detection surface reports false-positive
    // rates, but it is a behaviour CHANGE for a tab the user works in daily,
    // so it is a deliberate flip, not something Phase D does silently. Flip by
    // editing the values below (and note that a user's own project config
    // merges underneath, so this pin wins for `build`).
    //
    // PINNED SINCE #48 (M-16): `read`, as an OBJECT that restates the four
    // patterns above verbatim. Phase D left it out so upstream could add a new
    // secret-file pattern and have it reach the tab; that traded a real hole for
    // a hypothetical improvement, because last-match-wins applies to the
    // PROJECT config too and a cloned repo's `{"permission":{"read":"allow"}}`
    // resolved `read * → allow`, reading `.env` with no prompt (verified live).
    // The order inside the object is load-bearing — see
    // `opencode_pinned_read`'s doc. `external_directory` and `doom_loop` are
    // still left alone: their defaults are already `ask`, so the same trick
    // loosens nothing a project config could not loosen anyway by asking the
    // user, and pinning a per-directory allowlist here would freeze the cwd.
    //
    // Deliberately a CONSTANT, not a setting — same argument as
    // `subagent_depth` above: `spawn_inject_sig` only needs entries for
    // Settings-derived spawn injections.
    //
    // ── V32 Phase F (locked decision 14) ───────────────────────────────────
    // `native_web_visibility: "deny"` flips the two WEB values — and only
    // those two — to `"deny"`, closing OpenCode's own route to the network so
    // every fetch has to go through the proxied `ddg`/MCP tools, where the
    // taint latch actually works. `bash` and `edit` keep their pinned values in
    // every mode: shell-level egress (`curl`) is V33's problem (documented
    // honest limit), and taking `edit` away would gut the tab.
    //
    // ── V32 Phase G (locked decision 16) ───────────────────────────────────
    // Two independent switches meet on this one block, so it is assembled from
    // two independent decisions rather than emitted wholesale:
    //   * the PINS (`bash`/`edit`, and the web keys at their upstream values)
    //     are consumer hygiene — locked decision 8's drift-immunity;
    //   * the DENIALS are native-web visibility — locked decision 14's `deny`.
    // With hygiene off and `deny` on, the block carries the two denials and
    // nothing else: turning off "pin upstream defaults" must not also turn off
    // a deliberate denial, which is a different feature the user did not touch.
    // With both off, no `agent` key is written at all and OpenCode's own
    // defaults apply — exactly the pre-V32 posture the escape hatch promises.
    let native_web = native_web_for(settings, "opencode", tab);
    let hygiene = consumer_hygiene_for(settings, "opencode", tab);
    let denied = native_web == NativeWebVisibility::Deny;
    if hygiene || denied {
        let mut permission = serde_json::Map::new();
        if hygiene {
            permission.insert("bash".into(), OPENCODE_PINNED_BASH.into());
            permission.insert("edit".into(), OPENCODE_PINNED_EDIT.into());
            // #48 (M-16). After `edit` and before the web keys so the emitted
            // block reads in the same order the constants are declared.
            permission.insert("read".into(), opencode_pinned_read());
        }
        if denied {
            permission.insert("webfetch".into(), OPENCODE_DENIED.into());
            permission.insert("websearch".into(), OPENCODE_DENIED.into());
        } else if hygiene {
            permission.insert("webfetch".into(), OPENCODE_PINNED_WEBFETCH.into());
            permission.insert("websearch".into(), OPENCODE_PINNED_WEBSEARCH.into());
        }
        config.insert(
            "agent".to_string(),
            serde_json::json!({ "build": { "permission": permission } }),
        );
    }

    // Build the `mcp` object from up to two stdio children (mirrors the
    // two-server `--mcp-config` map in `build_pre_args`):
    //   - `cimp-offload` carries `offload_task`, the `graph_*` tools, and any
    //     OpenCode-exposed MCP server — **V37 Phase F: UNCONDITIONAL**.
    //   - V26 `cimp-code-audit` carries `security_audit` / `quality_audit` —
    //     injected when Code Audit is enabled AND `expose_opencode` is on.
    // The `mcp` key is emitted only if at least one server made the cut — which,
    // since Phase F, is every OpenCode tab whose exe path resolves.
    if let Ok(exe) = std::env::current_exe() {
        let exe = exe.to_string_lossy().to_string();
        let mut mcp = serde_json::Map::new();
        {
            // V37 Phase F: unconditional, for the reasons written out in full on
            // the Claude side (`harness::claude::overlay::build_pre_args`) —
            // OpenCode reaches this same child over stdio, so it inherits the
            // whole argument. Without a child there is no `tools/listChanged`
            // relay, and OpenCode's same-session refresh (the one live-verify
            // that made the V37 no-restart story true) has nothing to refresh.
            //
            // V28: see the Claude-side `--tab` note in `build_pre_args`.
            mcp.insert(
                "cimp-offload".to_string(),
                serde_json::json!({
                    "type": "local",
                    "command": [exe, "--offload-mcp", "--consumer", "opencode", "--tab", tab]
                }),
            );
        }
        if crate::harness::plugin::audit_advertised(settings, super::harness_plugin::me()) {
            mcp.insert(
                "cimp-code-audit".to_string(),
                serde_json::json!({
                    "type": "local",
                    // V32 C-1b: see the Claude-side note in `build_pre_args` —
                    // the tab identity is what gives `/audit/run` a latch to
                    // gate on.
                    "command": [exe, "--code-audit-mcp", "--consumer", "opencode", "--tab", tab]
                }),
            );
        }
        if !mcp.is_empty() {
            config.insert("mcp".to_string(), serde_json::Value::Object(mcp));
        }
    }

    // Reference the managed instructions file when any guidance applies. The
    // file itself is written at launch (see `build_ai_tool_spec`).
    // NOTE: `instructions` is emitted as an array-of-paths (the documented
    // shape); confirm against the live schema at F1 alongside the provider
    // block — if OpenCode silently ignores it, the TTS/offload/graph guidance
    // never reaches the session (no launch error surfaces).
    if !compose_capability_guidance(cfg, settings).is_empty() {
        let path = opencode_instructions_path(cfg);
        config.insert(
            "instructions".to_string(),
            serde_json::json!([path.to_string_lossy()]),
        );
    }

    // V21: inject the `local-llama` custom provider + select it as the default
    // `model` when one has been registered (Offload settings "Add to OpenCode",
    // or kept in sync by auto-sync). The OpenCode tab still uses OpenCode's own
    // global providers for everything else; this only adds the local
    // `llama-server`'s OpenAI-compatible endpoint and points `model` at it so a
    // freshly opened tab is ready to work. `None` ⇒ no `provider`/`model` keys,
    // exactly as before (default install / never registered).
    if let Some(provider) = super::settings::resolve_provider(settings) {
        if !provider.base_url.is_empty() && !provider.model.is_empty() {
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String(provider.base_url),
            );
            if !provider.api_key.is_empty() {
                options.insert(
                    "apiKey".to_string(),
                    serde_json::Value::String(provider.api_key),
                );
            }
            let mut models = serde_json::Map::new();
            models.insert(
                provider.model.clone(),
                serde_json::json!({ "name": provider.model.clone() }),
            );
            config.insert(
                "provider".to_string(),
                serde_json::json!({
                    "local-llama": {
                        "npm": "@ai-sdk/openai-compatible",
                        "name": "Local Llama (cImp offload)",
                        "options": serde_json::Value::Object(options),
                        "models": serde_json::Value::Object(models),
                    }
                }),
            );
            config.insert(
                "model".to_string(),
                serde_json::Value::String(format!("local-llama/{}", provider.model)),
            );
        }
    }
    serde_json::Value::Object(config)
}

// ── the local-provider block (V40 Phase E, locked decision 26) ──────────────
//
// Moved verbatim from `offload/server.rs`, where it was the last OpenCode
// config writer in core: a function that parses the offload server's own
// command line and emits ONE harness's provider block. The parsing helpers it
// uses stay in `offload/server.rs` — they read cImp's own llama-server command,
// which is not this harness's business — and are `pub(crate)` for this caller.

/// The [`ConfigWriter`] the descriptor hands core, so the "Add to OpenCode"
/// button can ask *the harness* rather than call a function named after it.
pub static WRITER: OpencodeConfigWriter = OpencodeConfigWriter;

/// See [`WRITER`]. A ZST, like the plugin itself.
pub struct OpencodeConfigWriter;

impl crate::harness::plugin::ConfigWriter for OpencodeConfigWriter {
    fn derive_local_provider(&self, server_command: &str) -> AppResult<LocalProviderBlock> {
        derive_provider(server_command)
    }
}

/// Derive the OpenCode `local-llama` provider from a Local backend's
/// `server_command`. Requires an explicit `--port` and a model identifier
/// (`--alias`/`-a`, else the `--model`/`-m` file basename); the host defaults
/// to `127.0.0.1`. On a missing required flag, returns a self-contained error
/// naming exactly what's absent so the Settings button can surface it verbatim.
pub fn derive_provider(command: &str) -> AppResult<LocalProviderBlock> {
    let tokens = shlex::split(command)
        .ok_or_else(|| AppError::Offload("server command has unbalanced quotes".into()))?;
    let mut it = tokens.into_iter();
    let _program = it
        .next()
        .ok_or_else(|| AppError::Offload("server command is empty".into()))?;
    let args: Vec<String> = it.collect();

    let mut host = server::DEFAULT_HOST.to_string();
    let mut port: Option<u16> = None;
    let mut alias: Option<String> = None;
    let mut model_path: Option<String> = None;
    // One definition of "where the key is", shared with `resolve_local_auth`.
    let api_key = server::api_key_from_args(&args);

    let mut i = 0;
    while i < args.len() {
        let (key, inline) = server::split_flag(&args[i]);
        match key {
            "--host" => {
                if let Some(v) = server::flag_value(inline, &args, &mut i) {
                    host = server::normalize_host(&v);
                }
            }
            "--port" => {
                if let Some(v) = server::flag_value(inline, &args, &mut i) {
                    if let Ok(p) = v.parse::<u16>() {
                        port = Some(p);
                    }
                }
            }
            "-a" | "--alias" => {
                if let Some(v) = server::flag_value(inline, &args, &mut i) {
                    if !v.trim().is_empty() {
                        alias = Some(v.trim().to_string());
                    }
                }
            }
            "-m" | "--model" => {
                if let Some(v) = server::flag_value(inline, &args, &mut i) {
                    if !v.trim().is_empty() {
                        model_path = Some(v);
                    }
                }
            }
            // `--api-key` is read by `api_key_from_args` above, not here — one
            // parser, two callers. It must still be *skipped* correctly so its
            // value cannot be mistaken for a positional model path.
            "--api-key" | "--api_key" => {
                let _ = server::flag_value(inline, &args, &mut i);
            }
            _ => {}
        }
        i += 1;
    }

    let model = alias.or_else(|| model_path.as_deref().map(model_id_from_path));

    // Collect every missing required param so the error names them all at once.
    let mut missing: Vec<&str> = Vec::new();
    if port.is_none() {
        missing.push("--port");
    }
    if model.is_none() {
        missing.push("a model (--model/-m or --alias/-a)");
    }
    if !missing.is_empty() {
        return Err(AppError::Offload(format!(
            "can't register the OpenCode local-llama provider: the server command is missing {}.",
            missing.join(" and ")
        )));
    }

    Ok(LocalProviderBlock {
        base_url: format!("http://{host}:{}/v1", port.expect("port present")),
        model: model.expect("model present"),
        api_key,
        source_command: command.to_string(),
    })
}

/// The OpenCode model id for a `--model` path: the file name with any leading
/// directory and a trailing `.gguf` removed
/// (`…/Qwen3.6-35B-A3B-Q4.gguf` → `Qwen3.6-35B-A3B-Q4`).
pub(crate) fn model_id_from_path(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    base.strip_suffix(".gguf")
        .or_else(|| base.strip_suffix(".GGUF"))
        .unwrap_or(base)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // The rest of this module's tests live in `tabs::config`'s test module (see
    // the note there): they drive the emitted config through the tab-spawn
    // composition and share ~30 helpers with it. These four are pure functions
    // over a string, so they live with the code.

    /// The generated password can never be the value that DISABLES auth.
    #[test]
    fn a_generated_server_password_is_never_empty_and_never_repeats() {
        let a = new_server_password();
        let b = new_server_password();
        assert!(!a.is_empty(), "an empty password disables auth upstream");
        assert_eq!(a.len(), 32, "32 hex chars of UUIDv4 entropy: {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "the password must be per spawn, not per build");
    }

    /// The header is `Basic base64("opencode:<password>")` — and an empty
    /// password yields NO header rather than a header for the empty string,
    /// because upstream reads an empty password as "auth off".
    #[test]
    fn the_basic_header_encodes_the_username_pair_and_refuses_an_empty_password() {
        use base64::prelude::*;
        let header = server_basic_auth("s3cret").expect("a non-empty password has a header");
        let encoded = header
            .strip_prefix("Basic ")
            .expect("the scheme is Basic, not Bearer");
        assert_eq!(
            String::from_utf8(BASE64_STANDARD.decode(encoded).expect("base64")).expect("utf8"),
            format!("{SERVER_USERNAME}:s3cret"),
        );
        assert_eq!(server_basic_auth(""), None);
    }

    /// The reader's credential comes from the child's COMPOSED environment, so a
    /// per-tab override (which wins at spawn) cannot leave the tap
    /// authenticating with a password the server never saw.
    #[test]
    fn the_readers_credential_follows_the_childs_effective_environment() {
        let mut env: HashMap<String, String> = HashMap::new();
        assert_eq!(server_auth_from_env(&env), None, "no variable ⇒ no header");
        env.insert(SERVER_PASSWORD_ENV.to_string(), String::new());
        assert_eq!(
            server_auth_from_env(&env),
            None,
            "an empty password disables auth upstream, so it must not produce a header"
        );
        env.insert(SERVER_PASSWORD_ENV.to_string(), "theirs".to_string());
        assert_eq!(
            server_auth_from_env(&env),
            server_basic_auth("theirs"),
            "the tap must authenticate with the password the CHILD will use"
        );
    }

    /// The two variables are set as a pair, with the username pinned to the
    /// value the header is built from.
    #[test]
    fn the_spawn_env_pairs_the_password_with_the_username_it_is_encoded_under() {
        let pairs = server_auth_env("pw");
        assert_eq!(
            pairs,
            [
                (SERVER_PASSWORD_ENV.to_string(), "pw".to_string()),
                (SERVER_USERNAME_ENV.to_string(), SERVER_USERNAME.to_string()),
            ]
        );
    }
}
