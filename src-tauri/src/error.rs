use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AppError {
    #[error("PTY operation failed: {0}")]
    Pty(String),

    #[error("failed to spawn subprocess: {0}")]
    Spawn(String),

    #[error("`{0}` executable not found on PATH")]
    CommandNotFound(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("IPC error: {0}")]
    Ipc(String),

    #[error("PTY session not started")]
    NotStarted,

    #[error("PTY session already started")]
    AlreadyStarted,

    #[error("audio error: {0}")]
    Audio(String),

    #[error("TTS error: {0}")]
    Tts(String),

    #[error("STT error: {0}")]
    Stt(String),

    #[error("model file not found: {0}")]
    ModelNotFound(String),

    #[error("settings error: {0}")]
    Settings(String),

    /// V8-01: the offload `llama-server` is not running / not yet healthy
    /// when an operation needed it. Surfaced to Claude as a clear "enable
    /// or start offload in cImp" message rather than a hang.
    #[error("offload server not ready: {0}")]
    OffloadNotReady(String),

    /// V8-01: a generic offload failure (command parse, agent loop, MCP
    /// host, budget). Carries human-readable context for the IPC layer.
    #[error("offload error: {0}")]
    Offload(String),

    /// The agent loop finished but produced no usable answer — e.g. a
    /// thinking turn consumed the whole output budget and emitted only a
    /// `<think>` block that stripped to empty. Kept distinct from `Offload`
    /// so the service can retry a `thinking:on` run once with `auto` before
    /// surfacing it to the caller as a failed task.
    #[error("offload produced no answer: {0}")]
    OffloadNoAnswer(String),

    /// V9-01: a code-knowledge-graph index for the requested project is
    /// not built/ready yet. Surfaced to the caller as a clear "index
    /// building" message rather than blocking.
    #[error("graph index not ready: {0}")]
    GraphNotReady(String),

    /// V9-01: a generic code-knowledge-graph failure (parse, store,
    /// query, embedding). Carries human-readable context for the IPC layer.
    #[error("graph error: {0}")]
    Graph(String),

    /// V12 Phase A: a `run_check` structured-diagnostics run failed (spawn,
    /// I/O, or shell resolution). Carries human-readable context for the
    /// tool layer; a bad/absent checker binary reads as this, not a panic.
    #[error("check error: {0}")]
    Checks(String),

    /// V12 Phase B: `graph_impact`'s default (diff-vs-HEAD) mode needs a git
    /// repository at the project root. Kept distinct from [`AppError::Graph`]
    /// so the tool/UI layer can render a specific "requires git" hint instead
    /// of a generic index error.
    #[error("not a git repository: {0}")]
    NotAGitRepo(String),
}

pub type AppResult<T> = std::result::Result<T, AppError>;

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
