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
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};

use crate::error::{AppError, AppResult};
use crate::settings::{BackendTier, ToolScope};

use super::Backend;

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8080;

/// Connection facts ccImp needs from the user's `server_command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    /// The `llama-server` executable (first shlex token).
    pub program: String,
    /// All arguments after the program, verbatim — ccImp spawns these
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
    /// without it — ccImp warns rather than failing obscurely.
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
    /// (ccImp doesn't presume to know llama.cpp's full flag set).
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
        })
    }

    /// The shared HTTP client (reused for the agent loop's chat calls).
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Per-request working token budget: `(n_ctx / slots) *
    /// high_water_pct/100`, reserving headroom for reasoning + the final
    /// answer. `None` until `n_ctx` is discovered.
    ///
    /// NOTE (validate during impl, milestone risk c): assumes `/props`
    /// reports the *total* `n_ctx` and divides by `slots`. If a future
    /// llama.cpp reports per-slot `n_ctx`, drop the division here.
    pub fn per_slot_budget(&self, high_water_pct: u8) -> Option<u32> {
        let n = self.n_ctx()?;
        let per_slot = n / self.cmd.parallel.max(1);
        Some(per_slot.saturating_mul(high_water_pct.min(100) as u32) / 100)
    }

    /// One `GET /health` probe. `true` iff the server answered 200.
    pub async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.cmd.base_url());
        match self.client.get(&url).send().await {
            Ok(resp) => {
                let ok = resp.status().is_success();
                self.ready.store(ok, Ordering::Relaxed);
                ok
            }
            Err(_) => {
                self.ready.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// Poll `/health` until ready or `timeout` elapses, then read
    /// `/props` once to cache `n_ctx`. Returns an error (not a hang) if
    /// the server never reaches ready.
    pub async fn poll_until_ready(&self, timeout: Duration) -> AppResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.health_check().await {
                if let Err(e) = self.refresh_props().await {
                    warn!(error = %e, "offload: server healthy but /props read failed");
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(AppError::OffloadNotReady(format!(
                    "llama-server at {} did not become healthy within {}s",
                    self.cmd.base_url(),
                    timeout.as_secs()
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
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
                debug!(n_ctx = n, slots = self.cmd.parallel, "offload: discovered context window");
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
    fn per_slot_budget_divides_by_parallel() {
        let s = LlamaServer::new("llama-server --jinja -np 4").unwrap();
        s.n_ctx.store(160_000, Ordering::Relaxed);
        // 160000 / 4 = 40000, * 80% = 32000
        assert_eq!(s.per_slot_budget(80), Some(32_000));
    }
}
