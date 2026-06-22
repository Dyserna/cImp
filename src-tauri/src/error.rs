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
    /// or start offload in ccImp" message rather than a hang.
    #[error("offload server not ready: {0}")]
    OffloadNotReady(String),

    /// V8-01: a generic offload failure (command parse, agent loop, MCP
    /// host, budget). Carries human-readable context for the IPC layer.
    #[error("offload error: {0}")]
    Offload(String),
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
