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
//! **not** spawn the process: the read-only Offload Server tab's PTY
//! *is* `llama-server`; this type coordinates that lifecycle and reads
//! its health over HTTP.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{BackendTier, OpencodeLocalProvider, ToolScope};

use super::Backend;

const DEFAULT_HOST: &str = "127.0.0.1";
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
}

/// Split `--flag=value` into `("--flag", Some("value"))`; a bare
/// `--flag` yields `("--flag", None)`.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((k, v)) => (k, Some(v)),
        None => (arg, None),
    }
}

/// Resolve a flag's value: the inline `--flag=value` form, else the next
/// token (advancing `i`).
fn flag_value(inline: Option<&str>, args: &[String], i: &mut usize) -> Option<String> {
    if let Some(v) = inline {
        return Some(v.to_string());
    }
    *i += 1;
    args.get(*i).cloned()
}

/// Map a bind-all address to a loopback connect address.
fn normalize_host(host: &str) -> String {
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
        })
    }

    /// HTTP origin to reach the server (no trailing slash).
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// Derive the OpenCode `local-llama` provider from a Local backend's
/// `server_command`. Requires an explicit `--port` and a model identifier
/// (`--alias`/`-a`, else the `--model`/`-m` file basename); the host defaults
/// to `127.0.0.1`. On a missing required flag, returns a self-contained error
/// naming exactly what's absent so the Settings button can surface it verbatim.
pub fn derive_opencode_provider(command: &str) -> AppResult<OpencodeLocalProvider> {
    let tokens = shlex::split(command)
        .ok_or_else(|| AppError::Offload("server command has unbalanced quotes".into()))?;
    let mut it = tokens.into_iter();
    let _program = it
        .next()
        .ok_or_else(|| AppError::Offload("server command is empty".into()))?;
    let args: Vec<String> = it.collect();

    let mut host = DEFAULT_HOST.to_string();
    let mut port: Option<u16> = None;
    let mut alias: Option<String> = None;
    let mut model_path: Option<String> = None;
    let mut api_key = String::new();

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
                        port = Some(p);
                    }
                }
            }
            "-a" | "--alias" => {
                if let Some(v) = flag_value(inline, &args, &mut i) {
                    if !v.trim().is_empty() {
                        alias = Some(v.trim().to_string());
                    }
                }
            }
            "-m" | "--model" => {
                if let Some(v) = flag_value(inline, &args, &mut i) {
                    if !v.trim().is_empty() {
                        model_path = Some(v);
                    }
                }
            }
            "--api-key" | "--api_key" => {
                if let Some(v) = flag_value(inline, &args, &mut i) {
                    api_key = v;
                }
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

    Ok(OpencodeLocalProvider {
        base_url: format!("http://{host}:{}/v1", port.expect("port present")),
        model: model.expect("model present"),
        api_key,
        source_command: command.to_string(),
    })
}

/// The OpenCode model id for a `--model` path: the file name with any leading
/// directory and a trailing `.gguf` removed
/// (`…/Qwen3.6-35B-A3B-Q4.gguf` → `Qwen3.6-35B-A3B-Q4`).
fn model_id_from_path(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    base.strip_suffix(".gguf")
        .or_else(|| base.strip_suffix(".GGUF"))
        .unwrap_or(base)
        .to_string()
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
    /// Unreachable / transport error / non-2xx without a llama status.
    Down,
}

impl LlamaServer {
    /// Build a supervisor for the given `server_command` with V8-01
    /// defaults (`local` name, quality tier, all tools). A convenience over
    /// [`Self::with_config`] used by tests and any single-local caller.
    #[allow(dead_code)]
    pub fn new(command: &str) -> AppResult<Self> {
        Self::with_config("local", command, BackendTier::Quality, ToolScope::All)
    }

    /// Build a Local backend with an explicit name/tier/tool-scope (one
    /// pool entry). Parses the command (host/port/`-np`/`--jinja`) and
    /// warns if `--jinja` is absent. Does not contact the server — call
    /// [`Self::poll_until_ready`] after the tab's PTY has been spawned.
    pub fn with_config(
        name: &str,
        command: &str,
        tier: BackendTier,
        tool_scope: ToolScope,
    ) -> AppResult<Self> {
        let cmd = ServerCommand::parse(command)?;
        if !cmd.has_jinja {
            warn!(
                backend = name,
                "offload: server_command is missing `--jinja`; llama-server tool-calling \
                 will not work without it"
            );
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Offload(format!("failed to build HTTP client: {e}")))?;
        let parallel = cmd.parallel.max(1) as usize;
        Ok(Self {
            name: name.to_string(),
            tier,
            tool_scope,
            cmd,
            ready: AtomicBool::new(false),
            n_ctx: AtomicU32::new(0),
            gate: Arc::new(Semaphore::new(parallel)),
            client,
            last_error: StdMutex::new(None),
        })
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
        let resp = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => return HealthProbe::Down,
        };
        let is_2xx = resp.status().is_success();
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
            .client
            .get(&url)
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
    fn n_ctx(&self) -> Option<u32> {
        match self.n_ctx.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
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

    #[test]
    fn derive_provider_model_from_path_basename() {
        let p = derive_opencode_provider(
            "llama-server --model C:/models/Qwen3.6-35B-A3B-Q4.gguf --port 8080 --jinja",
        )
        .unwrap();
        assert_eq!(p.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(p.model, "Qwen3.6-35B-A3B-Q4");
        assert!(p.api_key.is_empty());
    }

    #[test]
    fn derive_provider_alias_wins_over_model_and_reads_host_apikey() {
        let p = derive_opencode_provider(
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
        let err = derive_opencode_provider("llama-server -a m")
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
        let err = derive_opencode_provider("llama-server --jinja")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--port"), "got: {err}");
        assert!(err.contains("--model/-m or --alias/-a"), "got: {err}");
    }
}
