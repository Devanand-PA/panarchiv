//! Error type shared by the whole core crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArchiveError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("archive error: {0}")]
    Message(String),
    #[error("document not found: {0}")]
    DocumentNotFound(String),
    #[error("item not found: {0}")]
    ItemNotFound(String),
    #[error("checksum mismatch for {name}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    #[error("OCR error: {0}")]
    Ocr(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, ArchiveError>;

pub fn msg<T: Into<String>>(s: T) -> ArchiveError {
    ArchiveError::Message(s.into())
}
