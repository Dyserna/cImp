use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
#[allow(dead_code)] // variants reserved for upcoming milestones
pub enum AppError {
    #[error("PTY operation failed: {0}")]
    Pty(String),

    #[error("failed to spawn `claude`: {0}")]
    Spawn(String),

    #[error("`claude` executable not found on PATH")]
    ClaudeNotFound,

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

    #[error("model file not found: {0}")]
    ModelNotFound(String),
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
