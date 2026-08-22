//! V8-01 offload server supervisor.
//!
//! [`ServerCommand`] parses (never mutates) the user's free-text
//! `server_command` to learn where to connect (`host`/`port`), how many
//! slots exist (`parallel`, from `-np`/`--parallel`), and whether
//! tool-calling will work (`has_jinja` — llama-server needs `--jinja`).
//!
//! [`LlamaServer`] is the single Local [`Backend`](super::Backend) impl.
//! It owns the HTTP *view* of the server — readiness, the discovered
//! context window (`n_ctx` from `/props`), and the in-flight/slot
//! accounting — and a concurrency gate sized to `parallel`. It does
//! **not** spawn the process: the supervisor owns `llama-server`; this
//! type coordinates that lifecycle and reads its health over HTTP.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{BackendTier, ToolScope};

use super::Backend;

pub(crate) const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

/// Connection facts cImp needs from the user's `server_command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    /// The `llama-server` executable (first shlex token).
    pub program: String,
    /// All arguments after the program, verbatim — cImp spawns these
    /// unchanged.
    pub args: Vec<String>,
    /// Host to connect to. `0.0.0.0`/`::` (bind-all) are normalized to
    /// `127.0.0.1` for the *connect* URL.
    pub host: String,
    /// Port to connect to (`--port`, default 8080).
    pub port: u16,
    /// Parallel slots (`-np`/`--parallel`, default 1). The context
    /// window divides across these.
    pub parallel: u32,
    /// Whether `--jinja` is present. Tool-calling silently won't work
    /// without it — cImp warns rather than failing obscurely.
    pub has_jinja: bool,
    /// Whether `-kvu`/`--kv-unified` is in effect (the explicit
    /// `-no-kvu`/`--no-kv-unified` turns it back off; last occurrence wins,
    /// as in llama.cpp's own parser). It changes what `/props` `n_ctx`
    /// *means* — see [`ServerCommand::per_slot_n_ctx`].
    pub kv_unified: bool,
}

/// Translate a `/props`-reported `n_ctx` into the window **one slot** can
/// actually hold.
///
/// llama-server reports `n_ctx` differently depending on the KV layout
/// (confirmed on build 10088 with `-c 8192 -np 2`):
///
/// - split KV (the default with an explicit `-np`): `n_ctx` is already the
///   per-slot window (`4096`) — return it unchanged.
/// - `--kv-unified`: `n_ctx` is the **full shared** window (`8192`), and
///   `/slots` echoes that same number for every slot — divide by the slot
///   count to get what a single request may occupy (`4096`).
///
/// With `parallel <= 1` the two are identical, so nothing is divided.
pub fn per_slot_n_ctx(reported: u32, parallel: u32, kv_unified: bool) -> u32 {
    if !kv_unified {
        return reported;
    }
    match parallel.max(1) {
        1 => reported,
        // `.max(1)` mirrors the other offload numerics: never hand a 0-token
        // window downstream (only reachable with an absurd `-np`).
        np => (reported / np).max(1),
    }
}

/// Split `--flag=value` into `("--flag", Some("value"))`; a bare
/// `--flag` yields `("--flag", None)`.
pub(crate) fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (arg, None),
    }
}

/// Resolve a flag's value: the inline `--flag=value` form, else the next
/// token (advancing `i`).
pub(crate) fn flag_value(inline: Option<&str>, args: &[String], i: &mut usize) -> Option<String> {
    if let Some(v) = inline {
        return Some(v.to_string());
    }
    *i += 1;
    args.get(*i).cloned()
}

/// The `--api-key` / `--api_key` value in an already-tokenized `llama-server`
/// argument list, or `""` when there is none. Last occurrence wins, as in
/// llama.cpp's own parser and as in every other flag this module reads.
///
/// **The single definition of "where the key is in a server command".** Two
/// callers need it and they must not disagree: [`derive_opencode_provider`]
/// (OpenCode's `local-llama` provider) and [`resolve_local_auth`] (cImp's own
/// bearer for the same server). A second parser is how the two would drift.
pub(crate) fn api_key_from_args(args: &[String]) -> String {
    let mut key = String::new();
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        if matches!(flag, "--api-key" | "--api_key") {
            if let Some(v) = flag_value(inline, args, &mut i) {
                key = v;
            }
        }
        i += 1;
    }
    key
}

/// The `--api-key` a `server_command` carries, or `""`. Tolerant by design:
/// an unparseable command (unbalanced quotes) yields no key rather than an
/// error, because every caller's fallback for "no key" is already correct.
pub fn api_key_from_command(command: &str) -> String {
    let Some(tokens) = shlex::split(command) else {
        return String::new();
    };
    // Skip the program; flags live after it.
    match tokens.split_first() {
        Some((_program, args)) => api_key_from_args(args),
        None => String::new(),
    }
}

/// The bearer token cImp actually sends to a Local backend, and where it came
/// from.
///
/// # Why the token has two sources (V33 stage 3)
///
/// `OffloadBackendKind::Local::auth_token` is *configured*; `--api-key` in the
/// same backend's `server_command` is what makes the server DEMAND a token, and
/// it is already parsed out for OpenCode's `local-llama` provider
/// (`opencode_provider.api_key`, [`derive_opencode_provider`]). So a user
/// securing a cImp-launched llama-server had to write the same secret in two
/// places, and getting it wrong in one of them made offload and OpenCode
/// disagree about a server they both talk to — with the symptom landing on
/// whichever one the user was not testing.
///
/// **Decision (2026-08-13): an empty `auth_token` falls back to the `--api-key`
/// already in the command.** The command is the stronger evidence of intent —
/// it is what the server will actually enforce — and the field stays available
/// to override it, which is the case that matters when cImp does not launch the
/// server (`autostart` off, an externally started keyed llama-server, no
/// `--api-key` for cImp to read).
///
/// Precedence, and the reason it runs this way round: an explicitly configured
/// token WINS. A user who typed a value into the field meant it, and silently
/// preferring the command string would make the field unable to correct a stale
/// key. Empty means "I did not choose", not "send nothing".
pub struct ResolvedAuth {
    /// The token to send; empty for none.
    pub token: String,
    /// True when [`Self::token`] came from `--api-key` rather than from the
    /// configured field. Carried so the "unauthorized" message can name the
    /// right place to fix — a user told to check a Settings field they left
    /// blank is being sent to the wrong screen.
    pub from_command: bool,
}

/// Resolve a Local backend's effective bearer — see [`ResolvedAuth`].
pub fn resolve_local_auth(configured: &str, server_command: &str) -> ResolvedAuth {
    let configured = configured.trim();
    if !configured.is_empty() {
        return ResolvedAuth {
            token: configured.to_string(),
            from_command: false,
        };
    }
    let derived = api_key_from_command(server_command);
    ResolvedAuth {
        from_command: !derived.is_empty(),
        token: derived,
    }
}

/// Map a bind-all address to a loopback connect address.
pub(crate) fn normalize_host(host: &str) -> String {
    match host {
        "0.0.0.0" | "::" | "[::]" => DEFAULT_HOST.to_string(),
        other => other.to_string(),
    }
}

impl ServerCommand {
    /// Parse a `server_command` string. Errors only when the command is
    /// empty or has unbalanced quotes; unrecognized flags are ignored
    /// (cImp doesn't presume to know llama.cpp's full flag set).
    pub fn parse(command: &str) -> AppResult<ServerCommand> {
        let tokens = shlex::split(command)
            .ok_or_else(|| AppError::Offload("server_command has unbalanced quotes".into()))?;
        let mut it = tokens.into_iter();
        let program = it
            .next()
            .ok_or_else(|| AppError::Offload("server_command is empty".into()))?;
        let args: Vec<String> = it.collect();

        let mut host = DEFAULT_HOST.to_string();
        let mut port = DEFAULT_PORT;
        let mut parallel = 1u32;
        let mut has_jinja = false;
        let mut kv_unified = false;

        let mut i = 0;
        while i < args.len() {
            let (key, inline) = split_flag(&args[i]);
            match key {
                "--host" => {
                    if let Some(v) = flag_value(inline, &args, &mut i) {
                        host = normalize_host(&v);
                    }
                }
                "--port" => {
                    if let Some(v) = flag_value(inline, &args, &mut i) {
                        if let Ok(p) = v.parse::<u16>() {
                            port = p;
                        }
                    }
                }
                "-np" | "--parallel" => {
                    if let Some(v) = flag_value(inline, &args, &mut i) {
                        if let Ok(n) = v.parse::<u32>() {
                            parallel = n.max(1);
                        }
                    }
                }
                "--jinja" => has_jinja = true,
                // Both spellings exist in llama.cpp (`-kvu, --kv-unified,
                // -no-kvu, --no-kv-unified`); later flags win.
                "-kvu" | "--kv-unified" => kv_unified = true,
                "-no-kvu" | "--no-kv-unified" => kv_unified = false,
                _ => {}
            }
            i += 1;
        }

        Ok(ServerCommand {
            program,
            args,
            host,
            port,
            parallel,
            has_jinja,
            kv_unified,
        })
    }

    /// HTTP origin to reach the server (no trailing slash).
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// This command's per-slot reading of a `/props` `n_ctx` — see the
    /// free [`per_slot_n_ctx`].
    pub fn per_slot_n_ctx(&self, reported: u32) -> u32 {
        per_slot_n_ctx(reported, self.parallel, self.kv_unified)
    }
}

/// The single Local backend: a supervised `llama-server`.
pub struct LlamaServer {
    /// Display/routing name (`local` for the V8-01 default).
    name: String,
    /// Which tier this backend serves (router bias).
    tier: BackendTier,
    /// Allow-list over the global tool pool.
    tool_scope: ToolScope,
    cmd: ServerCommand,
    ready: AtomicBool,
    /// Discovered `n_ctx` from `/props`; `0` means not-yet-known.
    n_ctx: AtomicU32,
    /// Concurrency gate sized to `cmd.parallel`. In-flight count is
    /// derived from `parallel - available_permits` (no separate
    /// counter to drift). `Arc` so we can `acquire_owned`.
    gate: Arc<Semaphore>,
    /// Short-timeout client for `/health` + `/props` probes.
    client: reqwest::Client,
    /// V33 Phase E: bearer token for this backend, or empty for none. Set on
    /// the probes below; the agent loop gets it separately, through
    /// `PoolEntry::auth_token` → `AgentConfig::auth_token`, because the chat
    /// call uses the service's own long-timeout client rather than this one.
    /// Never logged, never in an error string — see [`Self::auth_token`].
    ///
    /// **This is the EFFECTIVE token, not the configured field.** V33 stage 3:
    /// an empty configured token falls back to the `--api-key` in the same
    /// backend's `server_command` — see [`resolve_local_auth`].
    auth_token: String,
    /// Whether [`Self::auth_token`] was inherited from `--api-key` rather than
    /// typed into the backend's Auth token field. Reporting-only, and it exists
    /// for exactly one consumer: [`Self::unauthorized_message`], which would
    /// otherwise send a user to a Settings field they deliberately left blank.
    auth_from_command: bool,
    /// Last readiness failure reason (e.g. a non-llama.cpp server squatting
    /// on the port), surfaced to the Settings status row. `None` when ready
    /// or never probed.
    last_error: StdMutex<Option<String>>,
}

/// Outcome of a `/health` probe — distinguishes a real llama.cpp server from
/// a non-llama server (LM Studio, vLLM, …) that merely answers HTTP on the
/// same port. llama.cpp's `/health` always carries a `status` field
/// (`ok`/`loading model`/`error`); a generic OpenAI server does not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HealthProbe {
    /// llama.cpp reported `{"status":"ok"}` — ready to serve.
    Ready,
    /// A real llama.cpp, not yet ready (loading the model / transient error).
    Loading,
    /// Reachable + 2xx but NOT llama.cpp (no `status` field) — another app
    /// owns this port. A hard error, not worth waiting on.
    NotLlama,
    /// V33 Phase E: the server answered `401`/`403`. The endpoint is up and
    /// talking; cImp's credential is wrong or missing. Classified separately
    /// because it is otherwise indistinguishable from "down" (no `status`
    /// field, non-2xx) — and a user who has just turned on `--api-key` would
    /// be told their server never came up, which sends them debugging the
    /// wrong thing entirely.
    Unauthorized,
    /// Unreachable / transport error / non-2xx without a llama status.
    Down,
}

impl LlamaServer {
    /// Build a supervisor for the given `server_command` with V8-01
    /// defaults (`local` name, quality tier, all tools). A convenience over
    /// [`Self::with_config`] used by tests and any single-local caller.
    #[allow(dead_code)]
    pub fn new(command: &str) -> AppResult<Self> {
        Self::with_config("local", command, "", BackendTier::Quality, ToolScope::All)
    }

    /// Build a Local backend with an explicit name/tier/tool-scope (one
    /// pool entry). Parses the command (host/port/`-np`/`--jinja`) and
    /// logs a note if `--jinja` is absent (informational only — recent
    /// llama.cpp builds enable it by default). Does not contact the
    /// server — call [`Self::poll_until_ready`] after the tab's PTY has
    /// been spawned.
    pub fn with_config(
        name: &str,
        command: &str,
        auth_token: &str,
        tier: BackendTier,
        tool_scope: ToolScope,
    ) -> AppResult<Self> {
        let cmd = ServerCommand::parse(command)?;
        if !cmd.has_jinja {
            debug!(
                backend = name,
                "offload: server_command has no explicit `--jinja`; recent llama.cpp builds \
                 enable it by default, so this only matters on older builds — if tool-calling \
                 doesn't work, add `--jinja` explicitly"
            );
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Offload(format!("failed to build HTTP client: {e}")))?;
        let parallel = cmd.parallel.max(1) as usize;
        // V33 stage 3: `auth_token` here is the CONFIGURED field. An empty one
        // inherits the `--api-key` already in `command` — the flag that makes
        // this very server demand a token, and the one OpenCode's provider
        // reads for the same server. See `resolve_local_auth`.
        let auth = resolve_local_auth(auth_token, command);
        Ok(Self {
            name: name.to_string(),
            tier,
            tool_scope,
            cmd,
            ready: AtomicBool::new(false),
            n_ctx: AtomicU32::new(0),
            gate: Arc::new(Semaphore::new(parallel)),
            client,
            auth_token: auth.token,
            auth_from_command: auth.from_command,
            last_error: StdMutex::new(None),
        })
    }

    /// The effective bearer token, or `None` when there is none. Empty is
    /// treated as absent on purpose: sending a bare `Authorization: Bearer `
    /// is worse than sending nothing — some servers reject it outright, and it
    /// would break every existing unauthenticated setup.
    pub fn auth_token(&self) -> Option<&str> {
        if self.auth_token.is_empty() {
            None
        } else {
            Some(&self.auth_token)
        }
    }

    /// Attach the bearer token when there is one. Mirrors
    /// [`RemoteBackend::with_auth`](super::remote::RemoteBackend), which has
    /// carried the Remote half of this since V8-02.
    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.auth_token() {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    /// The last readiness failure reason (e.g. a non-llama.cpp server on the
    /// port), or `None` when ready / never failed.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    /// Probe `/health` and classify the responder (real llama.cpp vs. an
    /// impostor on the port vs. down).
    async fn probe_health(&self) -> HealthProbe {
        let url = format!("{}/health", self.cmd.base_url());
        let resp = match self.with_auth(self.client.get(&url)).send().await {
            Ok(r) => r,
            Err(_) => return HealthProbe::Down,
        };
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return HealthProbe::Unauthorized;
        }
        let is_2xx = status.is_success();
        let body = resp.text().await.unwrap_or_default();
        let llama_status = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
        match llama_status.as_deref() {
            Some("ok") => HealthProbe::Ready,
            // A real llama.cpp that isn't ready yet (`loading model`, `error`).
            Some(_) => HealthProbe::Loading,
            // 2xx but no llama.cpp `status` field → a different server owns
            // the port; non-2xx without one → just down.
            None if is_2xx => HealthProbe::NotLlama,
            None => HealthProbe::Down,
        }
    }

    /// The shared HTTP client (reused for the agent loop's chat calls).
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Per-request working token budget: `n_ctx * high_water_pct/100`,
    /// reserving headroom for reasoning + the final answer. `None` until
    /// `n_ctx` is discovered.
    ///
    /// llama.cpp's `/props` `default_generation_settings.n_ctx` is the
    /// **per-slot** context (the total `--ctx-size` already divided by
    /// `-np`), confirmed empirically (`--ctx-size 150000 -np 2` → `/props`
    /// n_ctx = 75008). So the discovered value *is* each slot's window — do
    /// **not** divide by `parallel` again (the V8-01 risk-c note resolved).
    /// The one exception is `--kv-unified`, where `/props` reports the full
    /// shared window instead; [`n_ctx`](Backend::n_ctx) already normalizes
    /// that back to per-slot, so this stays a plain percentage.
    pub fn per_slot_budget(&self, high_water_pct: u8) -> Option<u32> {
        let n = self.n_ctx()?;
        Some(n.saturating_mul(high_water_pct.min(100) as u32) / 100)
    }

    /// One `GET /health` probe. `true` iff a **real llama.cpp** server
    /// answered `{"status":"ok"}` — a non-llama server (LM Studio, …) that
    /// returns 200 for everything does *not* count as ready.
    pub async fn health_check(&self) -> bool {
        match self.probe_health().await {
            HealthProbe::Ready => {
                self.ready.store(true, Ordering::Relaxed);
                *self.last_error.lock().unwrap() = None;
                true
            }
            HealthProbe::NotLlama => {
                self.ready.store(false, Ordering::Relaxed);
                *self.last_error.lock().unwrap() = Some(self.not_llama_message());
                false
            }
            HealthProbe::Unauthorized => {
                self.ready.store(false, Ordering::Relaxed);
                *self.last_error.lock().unwrap() = Some(self.unauthorized_message());
                false
            }
            HealthProbe::Loading | HealthProbe::Down => {
                self.ready.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// The error shown when something other than llama.cpp owns the port.
    fn not_llama_message(&self) -> String {
        format!(
            "{} answered /health but is NOT a llama.cpp server. Another app is serving this port \
             (e.g. LM Studio / Ollama / vLLM). Free the port or point this Local backend at a real \
             llama-server — or add that server as a Remote backend instead.",
            self.cmd.base_url()
        )
    }

    /// The error shown when the server rejected cImp's credential. Names which
    /// of the **three** states cImp is in, because the fix differs in each —
    /// but never the token itself: this string reaches the Settings status row
    /// and the logs.
    ///
    /// The third state is V33 stage 3's: a token inherited from `--api-key`
    /// rather than typed into the Auth token field. That case has to say so, or
    /// it sends the user to a blank field to compare against a value that is not
    /// there — this is the surface that reports the backend's auth state, so it
    /// is where the fallback has to be visible.
    fn unauthorized_message(&self) -> String {
        let half = if self.auth_from_command {
            "cImp sent the `--api-key` from this backend's server command and it was rejected — \
             either that flag's value no longer matches what the server enforces, or set this \
             backend's Auth token explicitly to override it"
        } else if self.auth_token().is_some() {
            "cImp sent a bearer token and it was rejected — check that the backend's Auth token \
             matches the server's `--api-key`"
        } else {
            "cImp sent no bearer token — set this backend's Auth token to the server's \
             `--api-key`, or put `--api-key <token>` in its server command and cImp will use it"
        };
        format!(
            "{} rejected the request as unauthorized. {half}.",
            self.cmd.base_url()
        )
    }

    /// Poll `/health` until a real llama.cpp server is ready or `timeout`
    /// elapses, then read `/props` once to cache `n_ctx`. Fails **fast** with
    /// a clear message when a non-llama server owns the port (no point
    /// waiting), and on timeout otherwise — never hangs.
    pub async fn poll_until_ready(&self, timeout: Duration) -> AppResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.probe_health().await {
                HealthProbe::Ready => {
                    self.ready.store(true, Ordering::Relaxed);
                    *self.last_error.lock().unwrap() = None;
                    if let Err(e) = self.refresh_props().await {
                        warn!(error = %e, "offload: server healthy but /props read failed");
                    }
                    return Ok(());
                }
                HealthProbe::NotLlama => {
                    self.ready.store(false, Ordering::Relaxed);
                    let msg = self.not_llama_message();
                    *self.last_error.lock().unwrap() = Some(msg.clone());
                    return Err(AppError::Offload(msg));
                }
                // Also a hard error: waiting cannot make a wrong credential
                // right, and the timeout message would blame the wrong thing.
                HealthProbe::Unauthorized => {
                    self.ready.store(false, Ordering::Relaxed);
                    let msg = self.unauthorized_message();
                    *self.last_error.lock().unwrap() = Some(msg.clone());
                    return Err(AppError::Offload(msg));
                }
                HealthProbe::Loading | HealthProbe::Down => {
                    self.ready.store(false, Ordering::Relaxed);
                    if Instant::now() >= deadline {
                        let msg = format!(
                            "llama-server at {} did not become healthy within {}s",
                            self.cmd.base_url(),
                            timeout.as_secs()
                        );
                        *self.last_error.lock().unwrap() = Some(msg.clone());
                        return Err(AppError::OffloadNotReady(msg));
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    /// `GET /props` and cache `n_ctx` (the authoritative window,
    /// regardless of `--ctx-size`/`-c`/default).
    pub async fn refresh_props(&self) -> AppResult<()> {
        let url = format!("{}/props", self.cmd.base_url());
        let resp = self
            .with_auth(self.client.get(&url))
            .send()
            .await
            .map_err(|e| AppError::Offload(format!("/props request failed: {e}")))?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| AppError::Offload(format!("/props parse failed: {e}")))?;
        // llama-server nests it under default_generation_settings; some
        // builds expose it at the top level. Accept either.
        let n = v
            .get("default_generation_settings")
            .and_then(|g| g.get("n_ctx"))
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64()));
        match n {
            Some(n) => {
                self.n_ctx.store(n as u32, Ordering::Relaxed);
                debug!(
                    n_ctx = n,
                    slots = self.cmd.parallel,
                    kv_unified = self.cmd.kv_unified,
                    per_slot = self.cmd.per_slot_n_ctx(n as u32),
                    "offload: discovered context window"
                );
                Ok(())
            }
            None => Err(AppError::Offload("/props did not report n_ctx".into())),
        }
    }

    /// Acquire one offload slot, waiting up to `timeout` (which bounds
    /// queue-wait for a free slot). On timeout returns
    /// [`AppError::OffloadNotReady`] so the caller can surface a distinct
    /// "busy, retry later" result rather than treating it as a failure.
    pub async fn acquire_slot(&self, timeout: Duration) -> AppResult<OwnedSemaphorePermit> {
        match tokio::time::timeout(timeout, self.gate.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => Err(AppError::Offload("offload concurrency gate closed".into())),
            Err(_) => Err(AppError::OffloadNotReady(format!(
                "all {} offload slot(s) busy — timed out after {}s waiting for one",
                self.cmd.parallel,
                timeout.as_secs()
            ))),
        }
    }

    /// Mark the server not-ready (called when the tab's PTY exits or is
    /// torn down for a restart).
    pub fn mark_stopped(&self) {
        self.ready.store(false, Ordering::Relaxed);
        self.n_ctx.store(0, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = None;
    }
}

impl Backend for LlamaServer {
    fn name(&self) -> &str {
        &self.name
    }
    fn base_url(&self) -> String {
        self.cmd.base_url()
    }
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
    /// The window **one slot** can hold. The cached `/props` value stays raw;
    /// the `--kv-unified` correction is applied here, at the read, so every
    /// consumer (router budget, agent `max_tokens`/compaction, dashboards)
    /// sees the same per-slot number in both KV modes.
    fn n_ctx(&self) -> Option<u32> {
        match self.n_ctx.load(Ordering::Relaxed) {
            0 => None,
            n => Some(self.cmd.per_slot_n_ctx(n)),
        }
    }
    fn slots(&self) -> u32 {
        self.cmd.parallel
    }
    fn in_flight(&self) -> u32 {
        self.cmd
            .parallel
            .saturating_sub(self.gate.available_permits() as u32)
    }
    fn tier(&self) -> BackendTier {
        self.tier
    }
    fn tool_scope(&self) -> &ToolScope {
        &self.tool_scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_program_and_args() {
        let c = ServerCommand::parse("llama-server --model foo.gguf --port 9090 --jinja -np 4")
            .unwrap();
        assert_eq!(c.program, "llama-server");
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 9090);
        assert_eq!(c.parallel, 4);
        assert!(c.has_jinja);
        assert!(c.args.contains(&"--model".to_string()));
    }

    #[test]
    fn defaults_when_flags_absent() {
        let c = ServerCommand::parse("llama-server --model foo.gguf").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8080);
        assert_eq!(c.parallel, 1);
        assert!(!c.has_jinja);
    }

    #[test]
    fn inline_equals_form() {
        let c = ServerCommand::parse("llama-server --port=8000 --parallel=2").unwrap();
        assert_eq!(c.port, 8000);
        assert_eq!(c.parallel, 2);
    }

    #[test]
    fn bind_all_host_normalized_to_loopback() {
        let c = ServerCommand::parse("llama-server --host 0.0.0.0 --port 8080").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.base_url(), "http://127.0.0.1:8080");
    }

    #[test]
    fn windows_quoted_path() {
        let c = ServerCommand::parse(
            "\"C:/Program Files/llama/llama-server.exe\" --model \"C:/models/q4.gguf\" --jinja",
        )
        .unwrap();
        assert_eq!(c.program, "C:/Program Files/llama/llama-server.exe");
        assert!(c.has_jinja);
    }

    #[test]
    fn empty_command_errors() {
        assert!(ServerCommand::parse("").is_err());
        assert!(ServerCommand::parse("   ").is_err());
    }

    #[test]
    fn parallel_floored_at_one() {
        let c = ServerCommand::parse("llama-server -np 0").unwrap();
        assert_eq!(c.parallel, 1);
    }

    #[test]
    fn per_slot_budget_uses_props_value_directly() {
        let s = LlamaServer::new("llama-server --jinja -np 4").unwrap();
        // /props reports the PER-SLOT n_ctx already, so don't divide again:
        // 40000 * 80% = 32000.
        s.n_ctx.store(40_000, Ordering::Relaxed);
        assert_eq!(s.per_slot_budget(80), Some(32_000));
    }

    /// V33 Phase E. The Local backend's `/health` and `/props` probes carry
    /// `Authorization: Bearer …` when a token is configured — and, just as
    /// importantly, **carry no such header at all when one is not**. The whole
    /// installed base is unauthenticated, and an empty bearer is a credential
    /// some servers reject outright.
    #[test]
    fn probes_send_a_bearer_only_when_a_token_is_configured() {
        let header_of = |token: &str| -> Option<String> {
            let s = LlamaServer::with_config(
                "local",
                "llama-server --port 12344 --jinja",
                token,
                BackendTier::Quality,
                ToolScope::All,
            )
            .expect("parses");
            s.with_auth(s.client.get(format!("{}/health", s.cmd.base_url())))
                .build()
                .expect("a buildable request")
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .map(|v| v.to_str().expect("ascii").to_string())
        };
        assert_eq!(header_of(""), None, "no token ⇒ no header");
        assert_eq!(header_of("sk-local"), Some("Bearer sk-local".to_string()));
    }

    /// A rejected credential must not read as "the server never came up", and
    /// the message that says so must not quote the token — it lands in the
    /// Settings status row and the rolling log.
    #[test]
    fn the_unauthorized_message_names_the_cause_without_leaking_the_token() {
        let with = LlamaServer::with_config(
            "local",
            "llama-server --port 12344",
            "sk-local-secret",
            BackendTier::Quality,
            ToolScope::All,
        )
        .expect("parses");
        let msg = with.unauthorized_message();
        assert!(!msg.contains("sk-local-secret"), "{msg}");
        assert!(msg.contains("unauthorized"), "{msg}");
        assert!(msg.contains("rejected"), "{msg}");

        let without = LlamaServer::new("llama-server --port 12344").expect("parses");
        let msg = without.unauthorized_message();
        assert!(msg.contains("sent no bearer token"), "{msg}");

        // V33 stage 3 — the third state. A token inherited from `--api-key`
        // must NOT tell the user to check an Auth token field they left blank:
        // this is the surface that reports the backend's auth state, so the
        // fallback has to be visible in it.
        let inherited = LlamaServer::new("llama-server --port 12344 --api-key sk-from-cmd")
            .expect("parses");
        let msg = inherited.unauthorized_message();
        assert!(!msg.contains("sk-from-cmd"), "the token must not leak: {msg}");
        assert!(msg.contains("server command"), "{msg}");
        assert!(
            !msg.contains("check that the backend's Auth token matches"),
            "an inherited token must not send the user to a blank field: {msg}"
        );
    }

    // ── V33 stage 3 — `auth_token` falls back to the parsed `--api-key` ──────

    /// The three directions of the decision, at the resolver, in one place:
    /// an explicit token wins, an empty one inherits, neither means no header.
    /// Provenance is carried alongside, because the message the user reads
    /// depends on it.
    #[test]
    fn an_empty_auth_token_inherits_the_commands_api_key() {
        let cmd = "llama-server --port 12344 --api-key sk-from-cmd";

        let explicit = resolve_local_auth("sk-configured", cmd);
        assert_eq!(explicit.token, "sk-configured", "the field wins");
        assert!(!explicit.from_command);

        let inherited = resolve_local_auth("", cmd);
        assert_eq!(inherited.token, "sk-from-cmd", "empty inherits");
        assert!(inherited.from_command);

        // Whitespace is not a credential — otherwise a stray space in the field
        // would silently disable the fallback it was meant to leave alone.
        let blank = resolve_local_auth("   ", cmd);
        assert_eq!(blank.token, "sk-from-cmd");
        assert!(blank.from_command);

        let neither = resolve_local_auth("", "llama-server --port 12344");
        assert!(neither.token.is_empty(), "neither ⇒ no header at all");
        assert!(!neither.from_command);
    }

    /// The same three directions where they are actually observable: the
    /// `Authorization` header the probes send.
    #[test]
    fn the_probe_header_follows_the_resolved_token() {
        let header_of = |token: &str, cmd: &str| -> Option<String> {
            let s = LlamaServer::with_config(
                "local",
                cmd,
                token,
                BackendTier::Quality,
                ToolScope::All,
            )
            .expect("parses");
            s.with_auth(s.client.get(format!("{}/health", s.cmd.base_url())))
                .build()
                .expect("a buildable request")
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .map(|v| v.to_str().expect("ascii").to_string())
        };
        let keyed = "llama-server --port 12344 --api-key sk-from-cmd";
        assert_eq!(
            header_of("sk-configured", keyed),
            Some("Bearer sk-configured".to_string()),
            "an explicit token must not be overridden by the command"
        );
        assert_eq!(
            header_of("", keyed),
            Some("Bearer sk-from-cmd".to_string()),
            "an empty token inherits the command's --api-key"
        );
        assert_eq!(
            header_of("", "llama-server --port 12344"),
            None,
            "neither ⇒ no Authorization header (an empty bearer is worse than none)"
        );
        // Every spelling and form the one parser accepts, since OpenCode's
        // provider has always read them and the two must not diverge.
        for form in [
            "llama-server --port 12344 --api_key sk-from-cmd",
            "llama-server --port 12344 --api-key=sk-from-cmd",
            "llama-server --port 12344 --api-key sk-first --api-key sk-from-cmd",
        ] {
            assert_eq!(
                header_of("", form),
                Some("Bearer sk-from-cmd".to_string()),
                "form not handled: {form}"
            );
        }
    }

    /// The extracted parser is the SAME one OpenCode's provider uses — that is
    /// the point of extracting it. If these two ever disagree, offload and
    /// OpenCode authenticate differently against one server, which is the exact
    /// defect the fallback exists to remove.
    #[test]
    fn the_provider_and_the_backend_read_the_same_api_key() {
        for cmd in [
            "llama-server -a m --port 12344 --api-key sk-shared",
            "llama-server -a m --port 12344 --api_key=sk-shared",
            "llama-server -a m --port 12344",
        ] {
            let provider = crate::harness::opencode::config::derive_provider(cmd).expect("derives");
            assert_eq!(
                provider.api_key,
                api_key_from_command(cmd),
                "the two readers disagree for: {cmd}"
            );
            assert_eq!(
                resolve_local_auth("", cmd).token,
                provider.api_key,
                "the backend's inherited token must equal OpenCode's: {cmd}"
            );
        }
    }

    /// `--api-key`'s value must not be mistaken for something else by the
    /// provider's own walk now that its arm no longer consumes it explicitly.
    /// A model path is the one that would actually break (a positional read of
    /// the token as `-m`'s value), so it is the one pinned.
    #[test]
    fn the_api_key_value_is_not_read_as_another_flags_value() {
        let p = crate::harness::opencode::config::derive_provider(
            "llama-server --api-key /models/not-a-model.gguf --port 12344 -m /models/real.gguf",
        )
        .expect("derives");
        assert_eq!(p.model, "real", "the key's value is not the model");
        assert_eq!(p.api_key, "/models/not-a-model.gguf");
    }

    #[test]
    fn parses_kv_unified_flags() {
        let plain = ServerCommand::parse("llama-server -np 2").unwrap();
        assert!(!plain.kv_unified, "off unless asked for");
        for cmd in [
            "llama-server -np 2 --kv-unified",
            "llama-server -np 2 -kvu",
            // An explicit off, then on: the last occurrence wins.
            "llama-server -np 2 --no-kv-unified -kvu",
        ] {
            assert!(
                ServerCommand::parse(cmd).unwrap().kv_unified,
                "expected kv_unified for: {cmd}"
            );
        }
        for cmd in [
            "llama-server -np 2 --no-kv-unified",
            "llama-server -np 2 -no-kvu",
            "llama-server -np 2 --kv-unified --no-kv-unified",
        ] {
            assert!(
                !ServerCommand::parse(cmd).unwrap().kv_unified,
                "expected kv_unified off for: {cmd}"
            );
        }
    }

    #[test]
    fn kv_unified_budget_divides_by_parallel() {
        // Build 10088, `-c 8192 -np 2 --kv-unified`: /props reports the FULL
        // shared window (8192), so one slot only gets 4096 → 80% = 3276.
        let s = LlamaServer::new("llama-server --jinja -np 2 --kv-unified").unwrap();
        s.n_ctx.store(8_192, Ordering::Relaxed);
        assert_eq!(s.n_ctx(), Some(4_096));
        assert_eq!(s.per_slot_budget(80), Some(3_276));

        // Same command in split-KV mode: /props already reports 4096 and it
        // must pass through untouched.
        let split = LlamaServer::new("llama-server --jinja -np 2").unwrap();
        split.n_ctx.store(4_096, Ordering::Relaxed);
        assert_eq!(split.n_ctx(), Some(4_096));
        assert_eq!(split.per_slot_budget(80), Some(3_276));
    }

    #[test]
    fn kv_unified_without_parallel_does_not_divide() {
        // `-np` absent (llama.cpp's auto-slots default, which is where
        // kv_unified turns itself on) — one slot owns the whole window.
        let s = LlamaServer::new("llama-server --jinja --kv-unified").unwrap();
        s.n_ctx.store(8_192, Ordering::Relaxed);
        assert_eq!(s.n_ctx(), Some(8_192));
        assert_eq!(s.per_slot_budget(80), Some(6_553));

        // Explicit `-np 1` behaves the same.
        let one = LlamaServer::new("llama-server --jinja -np 1 -kvu").unwrap();
        one.n_ctx.store(8_192, Ordering::Relaxed);
        assert_eq!(one.n_ctx(), Some(8_192));
    }

    #[test]
    fn per_slot_n_ctx_edges() {
        // Split KV passes anything through verbatim, including the
        // not-yet-known `0` sentinel (the accessor maps that to `None` first).
        assert_eq!(per_slot_n_ctx(0, 4, false), 0);
        assert_eq!(per_slot_n_ctx(75_008, 2, false), 75_008);
        // Absurd `-np` can't produce a 0-token window.
        assert_eq!(per_slot_n_ctx(3, 8, true), 1);
        // `parallel` of 0 is treated as 1 (parse floors it anyway).
        assert_eq!(per_slot_n_ctx(8_192, 0, true), 8_192);
    }

    #[test]
    fn derive_provider_model_from_path_basename() {
        let p = crate::harness::opencode::config::derive_provider(
            "llama-server --model C:/models/Qwen3.6-35B-A3B-Q4.gguf --port 8080 --jinja",
        )
        .unwrap();
        assert_eq!(p.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.model, "Qwen3.6-35B-A3B-Q4");
        assert!(p.api_key.is_empty());
    }

    #[test]
    fn derive_provider_alias_wins_over_model_and_reads_host_apikey() {
        let p = crate::harness::opencode::config::derive_provider(
            "llama-server -m /m/q4.gguf -a my-alias --host 0.0.0.0 --port 9001 --api-key sk-x",
        )
        .unwrap();
        // alias beats the file basename; bind-all host normalized to loopback.
        assert_eq!(p.model, "my-alias");
        assert_eq!(p.base_url, "http://127.0.0.1:9001/v1");
        assert_eq!(p.api_key, "sk-x");
    }

    #[test]
    fn derive_provider_missing_port_errors_naming_it() {
        let err = crate::harness::opencode::config::derive_provider("llama-server -a m")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--port"), "got: {err}");
        assert!(
            !err.contains("model"),
            "port-only miss shouldn't name model: {err}"
        );
    }

    #[test]
    fn derive_provider_missing_both_names_both() {
        let err = crate::harness::opencode::config::derive_provider("llama-server --jinja")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--port"), "got: {err}");
        assert!(err.contains("--model/-m or --alias/-a"), "got: {err}");
    }
}
