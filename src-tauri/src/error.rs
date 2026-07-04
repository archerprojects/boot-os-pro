use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BootOsProError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Command failed ({cmd}): {stderr}")]
    CommandFailed { cmd: String, stderr: String },

    #[error("Operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl Serialize for BootOsProError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, BootOsProError>;
