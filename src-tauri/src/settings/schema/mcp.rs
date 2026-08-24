//! Server-command templates, command policies, the MCP registry and the
//! offload backend pool.
//!
//! Split out of `schema.rs` by V42 R10; see the module docs in `mod.rs`.

use super::*;

/// A named, reusable `llama-server` launch command the user can save from a
/// Local backend's `Server command` field in the Offload → Pool editor and
/// paste back into that field later. Stored globally in
/// [`OffloadSettings::server_command_templates`] so a library of commands
/// survives across backends and app restarts.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct ServerCommandTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// The saved `llama-server` command line, verbatim.
    pub command: String,
}

/// A named, reusable Remote-backend endpoint (base URL + auth token) the user
/// can save from a Remote backend's fields and paste back in later. Stored
/// globally in [`OffloadSettings::remote_backend_templates`]. The `auth_token`
/// may be a real bearer secret, so the hand-rolled `Debug` below redacts it.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct RemoteBackendTemplate {
    /// User-facing label, unique within the list (the UI enforces uniqueness).
    pub name: String,
    /// Saved backend base URL (e.g. `http://192.168.1.50:8080`).
    pub base_url: String,
    /// Saved bearer/auth token. Stored cleartext on disk (like the backend's
    /// own `auth_token`); redacted in `Debug`.
    pub auth_token: String,
}

impl std::fmt::Debug for RemoteBackendTemplate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteBackendTemplate")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// One environment variable forced at spawn by a [`CommandPolicy`] (e.g.
/// `GIT_PAGER=cat`). An ordered list of these — rather than a map — keeps the
/// settings diff deterministic and maps cleanly to the UI's key/value rows.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct CommandEnvVar {
    pub key: String,
    pub value: String,
}

/// A per-program command security policy applied by the native `run_command`
/// tool on top of the allowlist. Generalizes what used to be a hardcoded `git`
/// guard so any allowlisted program can be hardened, and so the rules are
/// visible/editable in Settings rather than buried in code.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct CommandPolicy {
    /// Program this policy hardens, matched against the allowlisted command by
    /// file-stem, case-insensitively (so `git` covers `git` and `git.exe`).
    pub program: String,
    /// Argument tokens to refuse. An argument matches when it equals the entry
    /// OR starts with `<entry>=` — covering `-c`, `--config-env`, `--git-dir`,
    /// … in both `--flag value` and `--flag=value` forms.
    pub denied_flags: Vec<String>,
    /// Subcommands (the first non-flag argument) to refuse, e.g. `config`.
    pub denied_subcommands: Vec<String>,
    /// V21 F7: when non-empty, an *allowlist* over the first non-flag argument —
    /// only these subcommands may run; every other (and a bare invocation) is
    /// refused. This is the strict counterpart to `denied_subcommands`: a
    /// denylist can't safely enumerate an open-ended set (a program's future or
    /// aliased subcommands would slip through), so a program that must be pinned
    /// to a few read-only verbs (e.g. `cargo` → `metadata`/`tree`, never
    /// `run`/`build`) uses this. Combined with denying the program's
    /// value-taking global flags (so the first non-flag token can't be shifted
    /// off the real subcommand), it can never reach a code-executing subcommand.
    /// Empty (the default) disables the allowlist and leaves only the denylist.
    pub allowed_subcommands: Vec<String>,
    /// Environment variables forced at spawn to neutralize config-driven hooks
    /// (e.g. `GIT_PAGER=cat`, empty `GIT_SSH_COMMAND`).
    pub env: Vec<CommandEnvVar>,
}

/// The seeded default policies. The `git` policy reproduces exactly the
/// hardening that `run_command` previously applied in code, so behavior is
/// unchanged on a fresh install — it's just visible and editable now. Because
/// [`OffloadSettings`] is `#[serde(default)]`, existing config files missing
/// the `command_policies` key inherit this automatically (no migration).
pub fn default_command_policies() -> Vec<CommandPolicy> {
    fn env(key: &str, value: &str) -> CommandEnvVar {
        CommandEnvVar {
            key: key.to_string(),
            value: value.to_string(),
        }
    }
    fn s(v: &str) -> String {
        v.to_string()
    }
    vec![CommandPolicy {
        program: s("git"),
        denied_flags: vec![
            s("-c"),
            s("-C"),
            s("--config-env"),
            s("--exec-path"),
            s("--git-dir"),
            s("--work-tree"),
            s("--upload-pack"),
            s("--receive-pack"),
            // The remaining value-consuming git globals. They aren't dangerous
            // themselves, but `dangerous_args` finds the subcommand as the first
            // non-flag token — so a non-denied value-taking global would shift
            // that token onto its own value and let `git --namespace x config …`
            // slip the `config` check. Denying every value-taking global keeps
            // that assumption sound (a read probe never needs these).
            s("--namespace"),
            s("--super-prefix"),
            s("--attr-source"),
        ],
        denied_subcommands: vec![s("config")],
        allowed_subcommands: vec![],
        env: vec![
            env("GIT_PAGER", "cat"),
            env("PAGER", "cat"),
            env("GIT_SSH_COMMAND", ""),
            env("GIT_TERMINAL_PROMPT", "0"),
            env("GIT_CONFIG_NOSYSTEM", "1"),
            env("GIT_CONFIG_GLOBAL", "/dev/null"),
        ],
    }]
}

/// V21 F7: the program names the curated "safe read-only commands" preset adds
/// to `command_allowlist` (deduped by the UI's merge action). `git` is already
/// hardened by [`default_command_policies`] (read probes allowed, exec/escape
/// vectors blocked); `cargo` is paired with [`readonly_cargo_policy`], which
/// pins it to `metadata`/`tree` — both resolve/read the dependency graph and
/// neither runs build scripts or project code. Deliberately excludes anything
/// that writes, fetches the network by default, or executes project code
/// (`npm`, `make`, bare `cargo`).
pub fn readonly_command_preset() -> Vec<String> {
    vec!["git".to_string(), "cargo".to_string()]
}

/// V21 F7: the `cargo` policy the read-only preset installs. Allowlists only
/// `metadata` and `tree`, and denies cargo's value-taking / code-executing
/// global flags (`--config` can inject a runner/wrapper; `-C` escapes the
/// working dir; `-Z` enables unstable behavior) — denying the value-taking
/// globals also keeps the `allowed_subcommands` check sound, since none of them
/// can shift the first-non-flag token off the real subcommand. `--explain` /
/// `--color` are denied only to preserve that soundness (both take a value);
/// their glued short-flag forms (`-Cdir`, `-Zflag`) are caught by the same
/// flag-denial machinery `git` uses.
pub fn readonly_cargo_policy() -> CommandPolicy {
    fn s(v: &str) -> String {
        v.to_string()
    }
    CommandPolicy {
        program: s("cargo"),
        denied_flags: vec![
            s("--config"),
            s("-C"),
            s("-Z"),
            s("--explain"),
            s("--color"),
        ],
        denied_subcommands: vec![],
        allowed_subcommands: vec![s("metadata"), s("tree")],
        env: vec![],
    }
}

/// V21 F7: merge the curated read-only preset into an existing `allowlist` +
/// `policies` in place. Idempotent (a merge-into-settings action, not a mode):
/// preset programs already present are not duplicated, and the `cargo` policy is
/// installed only if the user has no policy for `cargo` yet — so re-clicking the
/// button, or clicking it after hand-editing, never grows or resets what's
/// there. The user can freely prune any of it afterward. `git`'s policy is
/// seeded by [`default_command_policies`], so this only ever needs to add the
/// `cargo` one.
pub fn merge_readonly_preset(allowlist: &mut Vec<String>, policies: &mut Vec<CommandPolicy>) {
    for prog in readonly_command_preset() {
        if !allowlist.iter().any(|a| a.eq_ignore_ascii_case(&prog)) {
            allowlist.push(prog);
        }
    }
    if !policies
        .iter()
        .any(|p| p.program.eq_ignore_ascii_case("cargo"))
    {
        policies.push(readonly_cargo_policy());
    }
}

/// On/off toggles for the native baseline offload tools (built into
/// cImp, zero external deps). All default on so offload works with no
/// MCP servers installed.
#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct OffloadToolToggles {
    /// Bounded line/byte reads within an `allowed_root`.
    pub read_file: bool,
    /// Directory enumeration within an `allowed_root` — the ground-truth
    /// answer to "what files exist / how many" (V21).
    pub list_dir: bool,
    /// `grep`/`glob` across an `allowed_root`; matching paths + snippets.
    pub code_search: bool,
    /// Allowlisted, read-only command execution. Default true, but inert
    /// until `command_allowlist` is populated (deny-by-default).
    pub run_command: bool,
    /// V21: run one of the project's *configured* check commands (build /
    /// typecheck / lint / test) and get back deduplicated diagnostics — lets
    /// the worker prove build/test/lint claims instead of asserting them.
    /// Default true, but inert until the top-level `checks` array is
    /// non-empty (gated identically to the `run_check` MCP tool).
    pub run_check: bool,
}

impl Default for OffloadToolToggles {
    fn default() -> Self {
        Self {
            read_file: true,
            list_dir: true,
            code_search: true,
            run_command: true,
            run_check: true,
        }
    }
}

/// V37 (contract C2): where an MCP server's definition came from.
///
/// `External` is every server the user pasted into Settings — an endpoint cImp
/// neither ships nor supervises. `Internal` is reserved for servers cImp itself
/// manages (#41's managed bundle); nothing in V37 *hosts* one, the flag only
/// badges the row and scopes the V37 Phase E description screen, which applies
/// to external surfaces only.
///
/// Defaults to `External` — the same direction the V32 `toolclass.rs` table
/// takes for an unknown name, and the safe one: a server whose origin nobody
/// declared is treated as untrusted, not as cImp's own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum McpOrigin {
    /// A server cImp itself manages (reserved; see #41).
    Internal,
    /// A user-configured third-party endpoint. The default.
    #[default]
    External,
}

/// V37 (contract C1/C2): a user-created grouping of MCP servers, referenced by
/// server `name` (there is no parallel id space — the name IS the id).
///
/// The category list is **global-only**: a `Vec` is safe here precisely because
/// no project overlay ever writes it, so `deep_merge`'s replace-arrays-wholesale
/// rule (`persistence.rs`) can never half-merge two category lists. The
/// per-project surface is [`McpActivation`], which is maps for exactly that
/// reason.
///
/// Membership is many-to-many: a server may appear in several categories, and a
/// server in no category rides its own toggle alone. See the effective-enable
/// predicate `offload::mcp_host::effective_enable` — the single owner of that
/// rule for both advertisement and dispatch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct McpCategory {
    /// Unique, user-chosen. This IS the category's id: renaming a category
    /// creates a new identity, and any activation entry keyed by the old name
    /// becomes inert (it names a category that no longer exists).
    pub name: String,
    /// Member server names ([`McpServerConfig::name`]). A name with no matching
    /// server is simply ignored — membership is resolved against the live list.
    pub servers: Vec<String>,
    /// Global on/off for the whole category. A project overlay may override it
    /// through [`McpActivation::categories`].
    pub enabled: bool,
}

impl Default for McpCategory {
    fn default() -> Self {
        Self {
            name: String::new(),
            // A freshly-created category is ON: creating a group must never
            // silently take tools away from the surface (the V37 C2 migration
            // invariant in the small).
            enabled: true,
            servers: Vec::new(),
        }
    }
}

/// V37 (contract C2): the per-project activation overlay — the ONLY part of the
/// MCP registry a project settings overlay may carry.
///
/// Both halves are **maps keyed by name**, never arrays. `persistence::deep_merge`
/// merges JSON objects key-by-key but replaces arrays *wholesale*, so an array
/// here would mean a project that overrides one server silently discards every
/// other project-level entry the global file carried. As maps, a project's
/// `{"servers": {"ddg": false}}` overlays exactly that one key.
///
/// An entry is an OVERRIDE, not a copy: `Some(v)` wins over the global
/// `enabled`, absence inherits it. That is what lets the UI show
/// inherited-vs-overridden and offer a revert (delete the key).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct McpActivation {
    /// Per-category overrides, keyed by [`McpCategory::name`].
    pub categories: BTreeMap<String, bool>,
    /// Per-server overrides, keyed by [`McpServerConfig::name`].
    pub servers: BTreeMap<String, bool>,
}

/// One user-installed MCP tool server, aggregated by cImp's MCP host.
/// Mirrors Claude Code's own `mcpServers` entry shape: either a stdio
/// server (`command` + `args` + `env`) or an HTTP server (`url`). Only
/// read-class tools from each server are exposed this milestone.
///
/// The hand-rolled `Debug` redacts `env` values, which may carry API
/// keys, and (V33 Phase E) the HTTP transport's `auth_token`, so a stray
/// `?settings` log line cannot leak either.
// V37 F5: `PartialEq`/`Eq` because the registry is written THROUGH to the
// physical global settings file on save (`persistence::sync_mcp_registry_into`),
// and that write-through only rewrites the file when the array actually moved.
// Derived rather than hand-rolled: every field is connection- or
// identity-relevant, so "equal" must mean all of them, and a field added later
// must join the comparison without anyone remembering to.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct McpServerConfig {
    /// Display + namespacing prefix (e.g. `ddg`, `git`). Tools are
    /// exposed to the model as `<name>__<tool>`.
    pub name: String,
    /// Stdio transport: the server executable. Empty when `url` is set.
    pub command: String,
    /// Args for the stdio `command`.
    pub args: Vec<String>,
    /// Extra env for the stdio `command` (may carry secrets — redacted
    /// in `Debug`).
    pub env: HashMap<String, String>,
    /// HTTP transport: the server base URL. Empty when `command` is set.
    pub url: String,
    /// V33 Phase E: bearer token sent on every HTTP request to
    /// [`url`](Self::url) — `initialize`, `notifications/initialized`,
    /// `tools/list` and every `tools/call`. Empty (the default, and every
    /// pre-V33 settings file) = no `Authorization` header, i.e. exactly the
    /// pre-V33 behaviour, which is also the safe direction. Redacted in
    /// `Debug`; ignored by the stdio transport, where a secret belongs in
    /// [`env`](Self::env) instead.
    ///
    /// It is part of [`config_sig`](crate::offload::mcp_host::host_config_sig)
    /// (as a fingerprint, not cleartext) — without that, editing the token in
    /// Settings would never reconnect the server and the change would silently
    /// do nothing.
    pub auth_token: String,
    /// **Per-harness exposure**, keyed by registry id — V40 Phase B (locked
    /// decision 5), schema 36. Replaces the `claude_access` / `opencode_access`
    /// pair the 35 -> 36 migration copies into it.
    ///
    /// An absent key is *not exposed* ([`McpAccess::default`]), which is both
    /// the pre-existing default for a new server and the only safe answer to a
    /// grant question about a harness nobody has decided about. A key for an
    /// unregistered harness round-trips untouched, so a downgrade does not
    /// silently revoke a grant the user will get back on the next upgrade.
    ///
    /// Read through `offload::mcp_host::Consumer::wants`, never field by field:
    /// that is where the offload/audit consumers' conservative fold onto the
    /// same flags lives.
    pub access: BTreeMap<String, McpAccess>,
    /// Expose this server's tools to the **offload worker** (the local model,
    /// via the warm `McpHost`). This is the legacy `enabled` behavior.
    ///
    /// Not in [`Self::access`] on purpose: the worker is cImp's own in-process
    /// consumer, not a harness, and a map keyed by `HarnessId` has no honest
    /// slot for it (locked decision 25 — `Consumer::conservative_grant` stays
    /// core because it is a security default, not a harness fact).
    pub offload_access: bool,
    /// V37 (contract C2): where this server came from — see [`McpOrigin`].
    /// Metadata only in V37: it badges the Settings row and scopes the Phase E
    /// tool-description screen to external surfaces. Defaults to `external`.
    pub origin: McpOrigin,
    /// V37 (contract C2/C3): the server's own global on/off switch, orthogonal
    /// to the three per-consumer `*_access` flags.
    ///
    /// `*_access` answers *who may see this server*; `enabled` answers *does it
    /// exist at all right now*. A disabled server is not connected, not
    /// advertised to any consumer, and `call_for_consumer` refuses a stale call
    /// naming the disabled state (contract C4). A project overlay may override
    /// it through [`McpActivation::servers`]; membership of a disabled category
    /// can turn it off even when this is `true`.
    ///
    /// Defaults to `true` — the C2 migration invariant: every pre-v32 config's
    /// effective tool surface is unchanged after the upgrade.
    ///
    /// Part of `offload::mcp_host::config_sig`, so a toggle moves
    /// `host_config_sig` and `warm_host` actually reconciles.
    pub enabled: bool,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env_keys: Vec<&String> = self.env.keys().collect();
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("args", &self.args)
            // Redact values; show only which keys are present.
            .field("env_keys", &env_keys)
            .field("url", &self.url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("access", &self.access)
            .field("offload_access", &self.offload_access)
            .field("origin", &self.origin)
            .field("enabled", &self.enabled)
            .finish()
    }
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            url: String::new(),
            auth_token: String::new(),
            access: BTreeMap::new(),
            offload_access: true,
            // V37 C2: unknown provenance ⇒ external (untrusted), and a server
            // exists unless the user says otherwise.
            origin: McpOrigin::External,
            enabled: true,
        }
    }
}

/// V8-02: native + MCP-server tool names treated as **local-data** tools —
/// they read the user's files / run commands / query the local repo, so
/// their output must never leave the machine. The router refuses to send a
/// task needing any of these to a cloud backend, and a cloud backend's
/// default [`ToolScope`] denies them. (MCP servers are matched by their
/// configured `name`; native tools by their fixed name.)
///
/// #48, finding **F-12**: `run_check` joined this set. It executes the project's
/// **configured** build/test/lint commands and returns their output — which
/// quotes source — so it is closer to `run_command` than to anything on the
/// cloud-allowed side. Membership here only fixes a **new** backend (it is what
/// [`ToolScope::default_for`] excludes, and what the v29 → v30 migration
/// backfills into an existing "web/docs only" exclusion list). An already
/// configured backend whose scope does *not* name it — `ToolScope::All`, or a
/// hand-picked `AllExcept` — is fixed by the **call-time** half instead:
/// `BackendGate::admit`'s `run_check` rule plus the
/// [`Settings::checks_allow_remote_worker`] opt-in. Neither half is sufficient
/// alone; see `offload::backend_gate`.
///
/// Mirrored by hand in `src/lib/settings/types.ts` (`LOCAL_DATA_TOOLS`), which
/// is what the Settings window writes when a backend's cloud flag is toggled.
///
/// #48 finding **F-27**: that mirror went stale the day `run_check` was added
/// here, so the settings-side half of F-12 had no effect for a release. It is now
/// held by an `include_str!` tripwire —
/// `settings::frontend_mirrors::local_data_tools_mirror_is_current` — which
/// compares the two as SETS in both directions, so editing this list without
/// editing that file fails `cargo test`.
pub const LOCAL_DATA_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "code_search",
    "run_command",
    "run_check",
    "filesystem",
    "git",
];

/// V8-02: web/docs tool names a cloud backend is allowed by default — they
/// reach out to the public internet, not the user's machine. Kept as the
/// documented counterpart to [`LOCAL_DATA_TOOLS`] (the cloud-allow taxonomy)
/// even though no code path currently reads it.
#[allow(dead_code)]
pub const WEB_DOCS_TOOLS: &[&str] = &["duckduckgo", "fetch", "context7"];

/// V8-02: which capability tier a backend serves. The router prefers a
/// `Fast` backend for trivial single-pass work and a `Quality` backend for
/// real reasoning; Claude's `tier` hint on `offload_task` biases the choice.
#[derive(Clone, Copy, Serialize, Deserialize, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum BackendTier {
    /// Small/fast backend (e.g. an 8 GB LAN box): trivial, single-pass,
    /// small-context offloads only.
    Fast,
    /// Large/capable backend (the main model): real reasoning, big context,
    /// multi-tool loops. The default when unspecified.
    #[default]
    Quality,
}

/// V8-02: a backend's allow-list over the global tool pool (native tools +
/// configured MCP-server names). Only allowed tools are placed in the
/// `tools` array sent to that backend's model — the privacy boundary for
/// cloud backends.
#[derive(Clone, Serialize, Deserialize, Debug, Default, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum ToolScope {
    /// Every tool in the pool (Local + trusted LAN default).
    #[default]
    All,
    /// Only the named tools.
    Only { tools: Vec<String> },
    /// Every tool except the named ones (cloud default = `AllExcept` the
    /// local-data set).
    AllExcept { tools: Vec<String> },
}

impl ToolScope {
    /// Whether `tool` (a native tool name or an MCP-server name) is allowed
    /// under this scope. Matching is exact; MCP tools namespaced as
    /// `<server>__<tool>` are tested by their server prefix via
    /// [`Self::allows_namespaced`].
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            ToolScope::All => true,
            ToolScope::Only { tools } => tools.iter().any(|t| t == tool),
            ToolScope::AllExcept { tools } => !tools.iter().any(|t| t == tool),
        }
    }

    /// Whether a (possibly namespaced) tool id is allowed. An MCP tool is
    /// exposed to the model as `<server>__<tool>`; scopes are written in
    /// terms of native tool names and MCP *server* names, so we test the
    /// server prefix for namespaced ids and the whole id otherwise.
    pub fn allows_namespaced(&self, tool_id: &str) -> bool {
        let key = tool_id.split("__").next().unwrap_or(tool_id);
        self.allows(key)
    }

    /// The default scope for a backend of the given kind: Local and trusted
    /// LAN get everything; a cloud backend gets web/docs only (the
    /// local-data set denied) until the user explicitly opts in. The
    /// Settings UI applies this when a backend's cloud flag is toggled; the
    /// router/tests exercise it as the canonical default.
    #[allow(dead_code)]
    pub fn default_for(kind_is_cloud: bool) -> Self {
        if kind_is_cloud {
            ToolScope::AllExcept {
                tools: LOCAL_DATA_TOOLS.iter().map(|s| s.to_string()).collect(),
            }
        } else {
            ToolScope::All
        }
    }
}

/// V8-02: kind-specific configuration for one offload backend.
///
/// `Local` mirrors V8-01's single-server config (the command cImp owns +
/// spawns as a read-only tab). `Remote` is a `base_url` cImp only
/// health-checks and connects to — no process, no tab. The hand-rolled
/// `Debug` on [`OffloadBackend`] redacts **both** variants' `auth_token`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub enum OffloadBackendKind {
    /// cImp owns the process: the V8-01 `server_command` + `autostart` +
    /// the read-only Offload server dashboard (Tool Activity tab) with
    /// Start/Stop/Reset.
    Local {
        /// The single source-of-truth `llama-server` command (shlex-parsed
        /// to spawn; host/port/`-np`/`--jinja` parsed from it).
        server_command: String,
        /// Spawn at app launch and keep warm (else lazy on first offload).
        autostart: bool,
        /// When true, the Offload server dashboard's Start button shows the
        /// command in an editable confirm popup first; an edited command is
        /// used for that launch only and never persisted.
        #[serde(default)]
        show_command_on_start: bool,
        /// V33 Phase E: bearer token sent on every request cImp makes to this
        /// server — `/health`, `/props` and the agent loop's chat completions.
        /// Empty (the default, and every pre-V33 settings file) = no
        /// `Authorization` header at all, i.e. exactly the pre-V33 behaviour.
        /// Redacted in `Debug`; stored cleartext in `settings.json` like every
        /// other house secret (`ClaudeLocalSettings::auth_token` is the model).
        ///
        /// This is the CLIENT half only. A `llama-server` ignores auth unless it
        /// was launched with `--api-key`, so for a backend cImp itself spawns
        /// the same value must also appear as `--api-key <token>` in
        /// `server_command` — that is what makes the server demand it, and it is
        /// also where [`crate::offload::server::derive_opencode_provider`]
        /// already reads the key from for OpenCode's `local-llama` provider.
        ///
        /// **The effective token may come from the command rather than from
        /// this field** (V33 stage 3). Because the two halves above are the same
        /// secret, requiring it in both places was a trap: get one wrong and
        /// offload and OpenCode disagree about a server they both talk to. So an
        /// EMPTY value here falls back to the `--api-key` already parsed out of
        /// `server_command`. A non-empty value still wins — the field is how you
        /// override a stale or absent flag, which is the case that matters when
        /// cImp does not launch the server itself.
        ///
        /// **Do not read this field directly to decide what to send.**
        /// [`OffloadBackendKind::effective_auth_token`] is the resolver, and
        /// `crate::offload::server::resolve_local_auth` is what it and
        /// `LlamaServer::with_config` share.
        ///
        /// **`#[serde(default)]` is load-bearing and must stay.** Serde's
        /// container-level default does NOT apply to enum variants (this enum is
        /// `#[serde(tag = "type")]` with no container default), so without it
        /// every existing settings file — none of which carries the key — would
        /// fail to deserialize the whole `backends` array. Same reason
        /// `show_command_on_start` above carries its own.
        #[serde(default)]
        auth_token: String,
    },
    /// cImp holds a `base_url` (+ optional auth) and health-checks it; it
    /// cannot start/stop the process. A LAN box or a cloud API.
    Remote {
        /// HTTP origin of the OpenAI-compatible endpoint, e.g.
        /// `http://192.168.1.50:8080` or `https://api.example.com/v1`.
        base_url: String,
        /// Optional bearer token (cloud APIs). Redacted in `Debug`.
        auth_token: String,
        /// Marks a backend whose data leaves the user's machine/network.
        /// Gates the consent toggle, the distinct UI badge, and the
        /// default web/docs-only tool scope.
        is_cloud: bool,
        /// Explicit acknowledgement that offloading here sends task text
        /// (and any local-data tool results, if scoped in) to a third
        /// party. A cloud backend is unusable until this is `true`.
        cloud_consent: bool,
    },
    /// **V39 Phase C — a facade over an open AI tab** (locked decision 3).
    ///
    /// cImp owns neither a process nor a URL here: the "backend" is another
    /// harness tab that the delegation engine drives exactly as a user would.
    /// The requesting harness cannot tell it from an HTTP server — it sees the
    /// user-chosen backend name, `LAN` as the kind
    /// ([`crate::offload::mcp`]'s `backend_label`), and nothing else.
    ///
    /// **Never written by the user, never persisted.** Entries of this kind
    /// exist only in [`Settings::effective_offload_backends`], synthesized from
    /// every AI tab whose `delegation_role` is
    /// [`RemoteOffload`](DelegationRole::RemoteOffload). The backend editor
    /// lists them read-only ("configured on the tab"), and
    /// `a_synthesized_facade_never_reaches_the_persisted_backend_list` pins
    /// that a save round-trip cannot write one.
    HarnessTab {
        /// The worker tab's id. Internal: it keys the engine's registry and the
        /// readiness probe, and it is the ONE field that must never reach a
        /// driver-facing string — see `backend_label` and
        /// `the_live_description_names_the_backend_not_the_tab`.
        tab: String,
    },
}

impl OffloadBackendKind {
    /// The bearer token cImp actually sends to this backend, or `""` for none.
    ///
    /// **The one resolver.** Three call sites read a Local backend's credential
    /// and they must not disagree — the supervised pool
    /// (`offload::server::LlamaServer`, which reaches the same answer through
    /// `resolve_local_auth` because it also needs to know *where* the token came
    /// from), the Offload server dashboard's `/slots`+`/metrics` poller, and the
    /// headless `cimp --offload-mcp` child that runs its own agent loop when the
    /// app is unreachable. A backend authenticated in two of the three is a
    /// credential bug that only shows up on whichever path the user is not
    /// looking at.
    ///
    /// For `Local`, an empty configured token inherits the `--api-key` in
    /// `server_command` — see the field doc above and
    /// [`crate::offload::server::resolve_local_auth`]. For `Remote` there is no
    /// command to inherit from, so it is the configured token verbatim.
    pub fn effective_auth_token(&self) -> String {
        match self {
            OffloadBackendKind::Local {
                auth_token,
                server_command,
                ..
            } => crate::offload::server::resolve_local_auth(auth_token, server_command).token,
            OffloadBackendKind::Remote { auth_token, .. } => auth_token.clone(),
            // A facade is driven through a PTY, not over HTTP: there is no
            // request to authenticate and no credential to resolve. Empty is
            // the honest answer, and every caller already treats it as "send no
            // `Authorization` header at all".
            OffloadBackendKind::HarnessTab { .. } => String::new(),
        }
    }
}

/// V8-02: one backend in the offload pool.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export_to = "settings.ts"))]
pub struct OffloadBackend {
    /// Display + routing-log name (e.g. `main`, `lan-3070`, `cloud`).
    pub name: String,
    /// Per-backend enable toggle. Disabled backends are invisible to the
    /// router and the capability union.
    pub enabled: bool,
    /// Local (owned process) vs. Remote (health-checked URL).
    pub kind: OffloadBackendKind,
    /// Context window to assume when `/props` is unavailable (many cloud
    /// APIs don't expose it). Ignored for endpoints that report `n_ctx`.
    pub declared_context: Option<u32>,
    /// Model label to show when `/props` is unavailable (cosmetic).
    pub declared_model: String,
    /// Which tier this backend serves (router bias).
    pub tier: BackendTier,
    /// This backend's allow-list over the global tool pool.
    pub tool_scope: ToolScope,
}

impl std::fmt::Debug for OffloadBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact both variants' auth_token so a stray `?settings` log can't
        // leak one. (`server_command` is printed verbatim — it is a command
        // line, and a user who puts `--api-key` in it has already put the
        // secret somewhere argv-visible; see the Local `auth_token` doc.)
        let kind_dbg: String = match &self.kind {
            OffloadBackendKind::Local {
                server_command,
                autostart,
                show_command_on_start,
                auth_token,
            } => format!(
                "Local {{ server_command: {server_command:?}, autostart: {autostart}, show_command_on_start: {show_command_on_start}, auth_token: {} }}",
                if auth_token.is_empty() { "<none>" } else { "<redacted>" }
            ),
            OffloadBackendKind::Remote {
                base_url,
                auth_token,
                is_cloud,
                cloud_consent,
            } => format!(
                "Remote {{ base_url: {base_url:?}, auth_token: {}, is_cloud: {is_cloud}, cloud_consent: {cloud_consent} }}",
                if auth_token.is_empty() { "<none>" } else { "<redacted>" }
            ),
            // No secret of any kind on this variant — the whole value is a tab
            // id, which is already all over the logs.
            OffloadBackendKind::HarnessTab { tab } => format!("HarnessTab {{ tab: {tab:?} }}"),
        };
        f.debug_struct("OffloadBackend")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("kind", &format_args!("{kind_dbg}"))
            .field("declared_context", &self.declared_context)
            .field("declared_model", &self.declared_model)
            .field("tier", &self.tier)
            .field("tool_scope", &self.tool_scope)
            .finish()
    }
}

impl Default for OffloadBackendKind {
    fn default() -> Self {
        OffloadBackendKind::Local {
            server_command: String::new(),
            autostart: false,
            show_command_on_start: false,
            auth_token: String::new(),
        }
    }
}

impl Default for OffloadBackend {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            kind: OffloadBackendKind::default(),
            declared_context: None,
            declared_model: String::new(),
            tier: BackendTier::Quality,
            tool_scope: ToolScope::All,
        }
    }
}

impl OffloadBackend {
    /// A cloud backend that hasn't been consented to is not usable: the
    /// router skips it and the UI flags it. Non-cloud backends are always
    /// "consented".
    pub fn cloud_blocked(&self) -> bool {
        matches!(
            self.kind,
            OffloadBackendKind::Remote {
                is_cloud: true,
                cloud_consent: false,
                ..
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn readonly_preset_merge_is_idempotent() {
        // V21 F7: merging into empty settings adds git + cargo to the allowlist
        // and installs the cargo policy (git already has its default policy).
        let mut allowlist: Vec<String> = vec![];
        let mut policies = default_command_policies(); // seeds git
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert!(allowlist.iter().any(|a| a == "git"));
        assert!(allowlist.iter().any(|a| a == "cargo"));
        let cargo_policies = policies.iter().filter(|p| p.program == "cargo").count();
        assert_eq!(cargo_policies, 1, "cargo policy installed exactly once");
        let allowlist_after_one = allowlist.clone();
        let policies_after_one = policies.clone();

        // Re-merging is a no-op: no duplicate allowlist entries, no second cargo
        // policy — a merge action, not a mode.
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert_eq!(
            allowlist, allowlist_after_one,
            "allowlist unchanged on re-merge"
        );
        assert_eq!(
            policies, policies_after_one,
            "policies unchanged on re-merge"
        );

        // A hand-added `git` (any case) is not duplicated either.
        let mut hand = vec!["GIT".to_string()];
        let mut pol = default_command_policies();
        merge_readonly_preset(&mut hand, &mut pol);
        assert_eq!(
            hand.iter()
                .filter(|a| a.eq_ignore_ascii_case("git"))
                .count(),
            1
        );
    }

    #[test]
    fn readonly_preset_respects_a_user_cargo_policy() {
        // A user who already authored their own `cargo` policy keeps it — the
        // preset must not clobber or duplicate it.
        let mut allowlist: Vec<String> = vec![];
        let mut policies = vec![CommandPolicy {
            program: "cargo".to_string(),
            denied_flags: vec!["--frozen".to_string()],
            denied_subcommands: vec![],
            allowed_subcommands: vec![],
            env: vec![],
        }];
        merge_readonly_preset(&mut allowlist, &mut policies);
        assert_eq!(policies.iter().filter(|p| p.program == "cargo").count(), 1);
        // The kept policy is the user's, not the preset's.
        assert_eq!(policies[0].denied_flags, vec!["--frozen".to_string()]);
    }

    #[test]
    fn command_policy_missing_allowed_subcommands_deserializes() {
        // Backward compat: a config written before `allowed_subcommands` existed
        // must load with an empty allowlist (serde default), not fail.
        let pol: CommandPolicy = serde_json::from_value(json!({
            "program": "git",
            "denied_flags": ["-c"],
            "denied_subcommands": ["config"],
            "env": [],
        }))
        .expect("legacy policy without allowed_subcommands deserializes");
        assert!(pol.allowed_subcommands.is_empty());
    }

    #[test]
    fn local_backend_kind_defaults_show_command_on_start() {
        // A pre-existing config written before the field existed must
        // deserialize with the popup off (opt-in behavior).
        let kind: OffloadBackendKind = serde_json::from_value(json!({
            "type": "local",
            "server_command": "llama-server --port 8080 --jinja",
            "autostart": true,
        }))
        .expect("legacy local kind deserializes");
        match kind {
            OffloadBackendKind::Local {
                show_command_on_start,
                ..
            } => assert!(
                !show_command_on_start,
                "missing field must default to false"
            ),
            _ => panic!("expected Local kind"),
        }
    }

    #[test]
    fn local_backend_kind_defaults_auth_token() {
        // V33 Phase E, the trap this test exists for: `OffloadBackendKind` is
        // `#[serde(tag = "type")]` with NO container-level `#[serde(default)]`,
        // and serde's container default would not apply to enum variants even
        // if it had one. Without the FIELD-level `#[serde(default)]` on
        // `auth_token`, this deserialize fails — and since `backends` is a plain
        // `Vec<OffloadBackend>`, that failure takes the WHOLE settings file with
        // it, for every install in existence.
        let kind: OffloadBackendKind = serde_json::from_value(json!({
            "type": "local",
            "server_command": "llama-server --port 12344 --jinja",
            "autostart": true,
            "show_command_on_start": false,
        }))
        .expect("a pre-V33 local kind (no auth_token) must still deserialize");
        match kind {
            OffloadBackendKind::Local { auth_token, .. } => assert!(
                auth_token.is_empty(),
                "missing field must default to no token = no Authorization header"
            ),
            _ => panic!("expected Local kind"),
        }
        // …and the whole enclosing backend list, which is how it is really read.
        let backends: Vec<OffloadBackend> = serde_json::from_value(json!([{
            "name": "local",
            "enabled": true,
            "kind": { "type": "local", "server_command": "llama-server", "autostart": false },
        }]))
        .expect("a pre-V33 backends array must still deserialize");
        assert_eq!(backends.len(), 1);
    }

    /// V33 stage 3 — the resolver the dashboard poller and the headless
    /// `--offload-mcp` child both read, in all three directions.
    ///
    /// It matters that this is tested at the *settings* seam and not only in
    /// `offload::server`: those two call sites used to read the raw field, and a
    /// backend authenticated on the supervised path but not on the dashboard or
    /// the headless one is a credential bug that only shows up where nobody is
    /// looking.
    #[test]
    fn effective_auth_token_falls_back_to_the_commands_api_key() {
        let local = |token: &str, cmd: &str| OffloadBackendKind::Local {
            server_command: cmd.to_string(),
            autostart: false,
            show_command_on_start: false,
            auth_token: token.to_string(),
        };
        let keyed = "llama-server --port 12344 --api-key sk-from-cmd";
        assert_eq!(
            local("sk-configured", keyed).effective_auth_token(),
            "sk-configured",
            "an explicitly configured token wins"
        );
        assert_eq!(
            local("", keyed).effective_auth_token(),
            "sk-from-cmd",
            "an empty token inherits the command's --api-key"
        );
        assert_eq!(
            local("", "llama-server --port 12344").effective_auth_token(),
            "",
            "neither ⇒ no token, which is no Authorization header"
        );
        // Remote has no command to inherit from — the configured value, verbatim.
        let remote = OffloadBackendKind::Remote {
            base_url: "https://api.example.com".into(),
            auth_token: "sk-remote".into(),
            is_cloud: true,
            cloud_consent: true,
        };
        assert_eq!(remote.effective_auth_token(), "sk-remote");
    }

    #[test]
    fn offload_backend_debug_redacts_both_kinds_auth_token() {
        let local = OffloadBackend {
            kind: OffloadBackendKind::Local {
                server_command: "llama-server".into(),
                autostart: false,
                show_command_on_start: false,
                auth_token: "sk-local-secret".into(),
            },
            ..Default::default()
        };
        let dbg = format!("{local:?}");
        assert!(!dbg.contains("sk-local-secret"), "{dbg}");
        assert!(dbg.contains("auth_token: <redacted>"), "{dbg}");
        // An absent token says so rather than reading as a hidden one.
        let none = OffloadBackend::default();
        assert!(format!("{none:?}").contains("auth_token: <none>"));
    }

    #[test]
    fn mcp_server_config_defaults_and_redacts_auth_token() {
        // Additive over the container-level `#[serde(default)]`: a pre-V33
        // entry loads with no token, which is no `Authorization` header, which
        // is today's behaviour.
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "name": "ddg",
            "url": "http://172.21.1.11:17201/mcp",
            "offload_access": true,
        }))
        .expect("a pre-V33 mcp server entry deserializes");
        assert!(cfg.auth_token.is_empty());

        let with_token = McpServerConfig {
            auth_token: "sk-mcp-secret".into(),
            ..cfg
        };
        let dbg = format!("{with_token:?}");
        assert!(!dbg.contains("sk-mcp-secret"), "{dbg}");
        assert!(dbg.contains("auth_token: \"<redacted>\""), "{dbg}");
    }

    /// V37 contract C2. Both new server fields must default to the values that
    /// reproduce pre-v32 behaviour — a server EXISTS and its provenance is
    /// UNTRUSTED — because the v31 → v32 migration deliberately writes no data
    /// and leans entirely on these.
    #[test]
    fn mcp_server_config_defaults_origin_and_enabled() {
        let cfg: McpServerConfig = serde_json::from_value(json!({
            "name": "ddg",
            "url": "http://172.21.1.11:17201/mcp",
            "offload_access": true,
        }))
        .expect("a pre-V37 mcp server entry deserializes");
        assert!(cfg.enabled, "a pre-v32 server must stay on the surface");
        assert_eq!(cfg.origin, McpOrigin::External);
        // Neither is a secret; the redacted Debug shows both.
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("enabled: true"), "{dbg}");
        assert!(dbg.contains("origin: External"), "{dbg}");

        // Explicit values round-trip, and `origin` uses the lowercase wire form
        // the TS mirror (`src/lib/settings/types.ts`) writes.
        let internal: McpServerConfig = serde_json::from_value(json!({
            "name": "bundled",
            "command": "cimp-mcp",
            "origin": "internal",
            "enabled": false,
        }))
        .expect("an explicit v32 entry deserializes");
        assert_eq!(internal.origin, McpOrigin::Internal);
        assert!(!internal.enabled);
        let text = serde_json::to_string(&internal).unwrap();
        assert!(text.contains("\"origin\":\"internal\""), "{text}");
        let back: McpServerConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(back.origin, McpOrigin::Internal);
        assert!(!back.enabled);
    }
}
